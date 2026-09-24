//! A drain that starts before the module has registered.
//!
//! Between spawn and HELLO a supervised subc module has a process but no
//! connection, so a drain has nothing to send `module.draining` or a GOODBYE
//! over. The supervisor used to assume a subc child had been told over its
//! connection anyway: it waited the whole drain budget for an exit nobody had
//! asked for and then SIGKILLed a healthy module, while every consumer was
//! refused as `supervisor_not_live`. Two `ck module restart` calls in quick
//! succession were the easiest way in: the second one ran right after the first
//! had spawned its replacement, before that process sent HELLO.
//!
//! These tests run a real daemon (forwarding table, control plane, supervisor
//! handle) against the fake-aft stub, and read the daemon's log through a
//! process-wide capture because the log lines are part of what is asserted.

#[cfg(unix)]
use std::path::Path;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use subc_control::{ClientControlRequest, ModuleProtocol};
#[cfg(unix)]
use subc_daemon::test_support::TestTempDir;
use subc_daemon::{
    read_frame, write_frame, Frame, ModuleSpec, ModuleState, ModuleStatus, RestartPolicy,
    SupervisedModule, Supervisor, SupervisorHandle, SupervisorProcessLiveness,
};
#[cfg(unix)]
use subc_protocol::{BindIdentity, ErrorBody, RouteTarget};
use subc_protocol::{Flags, FrameType, Priority};
use tokio::{
    io::AsyncWriteExt,
    time::{sleep, timeout, Instant},
};
use tracing_subscriber::fmt::MakeWriter;

mod common;
use common::{
    connect_authed_client, start_test_daemon_with_process_liveness_and_supervisor, TestDaemon,
};

/// Long on purpose: a test that passes must not be able to pass because the
/// budget ran out, so every "exited promptly" bound below is far inside it.
const LONG_DRAIN_BUDGET: Duration = Duration::from_secs(20);
/// Hang guard for setup waits (spawn, connect, register), not a latency bound.
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);

struct Harness {
    daemon: TestDaemon,
    supervisor: Supervisor,
}

impl Harness {
    async fn start(name: &str, drain_timeout: Duration) -> Self {
        log_capture();
        let process_liveness = Arc::new(SupervisorProcessLiveness::new());
        let handle = SupervisorHandle::new();
        let daemon = start_test_daemon_with_process_liveness_and_supervisor(
            name,
            process_liveness.clone(),
            handle.clone(),
        )
        .await;
        let supervisor = Supervisor::new(
            Arc::clone(&daemon.registry),
            RestartPolicy::new(1, Duration::from_millis(100)),
        )
        .with_process_liveness(process_liveness)
        .with_forwarding(Arc::clone(&daemon.forwarding))
        .with_handle(handle)
        .with_drain_timeout(drain_timeout)
        .with_connection_file_path(daemon.connection_file_path.clone());
        Self { daemon, supervisor }
    }

    #[cfg(unix)]
    fn events_path(&self, module_id: &str) -> PathBuf {
        self.daemon
            .temp_dir
            .join(format!("{module_id}-events.jsonl"))
    }

    fn spawn(&self, module_id: &str, extra_env: &[(&str, String)]) -> SupervisedModule {
        let mut env = vec![("FAKE_AFT_MODULE_ID".to_string(), module_id.to_string())];
        env.extend(
            extra_env
                .iter()
                .map(|(key, value)| ((*key).to_string(), value.clone())),
        );
        self.supervisor
            .spawn(ModuleSpec {
                module_id: module_id.to_string(),
                program: PathBuf::from(env!("CARGO_BIN_EXE_fake-aft-stub")),
                args: Vec::new(),
                env,
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::Subc,
                overlap: Default::default(),
            })
            .unwrap()
    }
}

/// Two restarts back to back must leave exactly one fresh process, and that
/// process must not be killed.
///
/// `Restart` acks at initiation, so both calls succeed at once and the second
/// waits in the module's command queue. It is dequeued the moment the first
/// has spawned the replacement, before that process has sent HELLO (the stub's
/// HELLO delay makes that window certain rather than likely). Running it then
/// would drain the process the first restart just produced; the request was
/// already satisfied by that process and must be coalesced into it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn back_to_back_restarts_produce_one_fresh_process_that_is_not_killed() {
    let harness = Harness::start("drain-before-hello-double-restart", LONG_DRAIN_BUDGET).await;
    let module_id = "double-restart-module";
    let module = harness.spawn(module_id, &[("FAKE_AFT_HELLO_DELAY_MS", "300".to_string())]);
    let first = wait_for_status(&module, SETUP_TIMEOUT, |status| {
        status.state == ModuleState::Running && status.live
    })
    .await;
    assert_eq!(first.spawn_generation, 1);

    let mut control = connect_authed_client(&harness.daemon.connection_file_path)
        .await
        .unwrap();
    for corr in [11, 12] {
        write_frame(
            &mut control,
            &control_frame(
                corr,
                ClientControlRequest::SupervisorRestart {
                    module_id: module_id.to_string(),
                    drain_timeout_ms: None,
                },
            ),
        )
        .await
        .unwrap();
    }
    control.flush().await.unwrap();
    for _ in 0..2 {
        let ack = timeout(SETUP_TIMEOUT, read_frame(&mut control))
            .await
            .expect("restart ack timed out")
            .unwrap()
            .expect("control connection closed before the restart ack");
        assert_eq!(
            ack.header.ty,
            FrameType::Response,
            "both restarts are acked at initiation: {ack:?}"
        );
    }

    // Well inside the 20 s budget: before the fix the second restart held the
    // module in `Draining` for the whole budget, so it never read live here.
    let restarted = wait_for_status(&module, SETUP_TIMEOUT, |status| {
        status.state == ModuleState::Running && status.live && status.spawn_generation >= 2
    })
    .await;
    // Give a wrongly-run second restart time to show itself: with the stub's
    // 300 ms HELLO delay and 100 ms backoff, a third spawn would land well
    // within this.
    sleep(Duration::from_millis(1500)).await;
    let settled = module.status().unwrap();
    assert_eq!(
        settled.spawn_generation, 2,
        "two restarts requested before the first finished must produce exactly one \
         fresh process: {settled:?}"
    );
    assert_eq!(settled.pid, restarted.pid, "the fresh process must survive");
    assert_eq!(settled.state, ModuleState::Running);
    assert!(settled.live);

    let history = module.terminal_history();
    assert!(
        history
            .entries
            .iter()
            .all(|entry| entry.exit_signal != Some(9)),
        "no process of this module may have been SIGKILLed: {:?}",
        history.entries
    );
    assert_eq!(
        history.entries.len(),
        1,
        "only the original process exited: {:?}",
        history.entries
    );
    assert_logged(
        module_id,
        "restart already satisfied by generation 2",
        Duration::from_secs(1),
    )
    .await;

    module.stop().await.unwrap();
}

/// A drain that finds no connection asks the child to stop by signal, so a
/// child that has not registered yet exits at once instead of sitting out the
/// budget and being SIGKILLed.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drain_before_hello_stops_the_child_by_sigterm_well_inside_the_budget() {
    let harness = Harness::start("drain-before-hello-sigterm", LONG_DRAIN_BUDGET).await;
    let module_id = "pre-hello-sigterm-module";
    let events = harness.events_path(module_id);
    // No SIGTERM handler: the default disposition ends the process with
    // signal 15, which is the witness that the supervisor sent it.
    let module = harness.spawn(
        module_id,
        &[
            ("FAKE_AFT_HELLO_DELAY_MS", "60000".to_string()),
            ("FAKE_AFT_EVENTS_PATH", path_string(&events)),
        ],
    );
    wait_for_event(&events, "hello_delay_started", SETUP_TIMEOUT).await;
    let running = wait_for_status(&module, SETUP_TIMEOUT, |status| {
        status.state == ModuleState::Running && status.pid.is_some()
    })
    .await;
    assert!(
        !running.registration_active,
        "the child must not have registered"
    );

    let started = Instant::now();
    module.restart(None).await.unwrap();
    let exit = wait_for_terminal(&module, Duration::from_secs(5)).await;
    let elapsed = started.elapsed();
    assert_eq!(
        exit.exit_signal,
        Some(15),
        "the unregistered child must be stopped by SIGTERM, not killed: {exit:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the child must exit well inside the {LONG_DRAIN_BUDGET:?} budget, took {elapsed:?}"
    );
    assert_logged(
        module_id,
        "module has no connection yet; requesting stop by signal",
        Duration::from_secs(1),
    )
    .await;

    module.stop().await.unwrap();
}

/// A child that ignores the signal still gets killed when the budget runs out,
/// and that kill is logged instead of showing only as signal 9 in the ring.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drain_budget_expiring_into_sigkill_is_logged_as_a_warning() {
    let budget = Duration::from_millis(1500);
    let harness = Harness::start("drain-before-hello-budget-warn", budget).await;
    let module_id = "pre-hello-ignores-sigterm";
    let events = harness.events_path(module_id);
    let module = harness.spawn(
        module_id,
        &[
            ("FAKE_AFT_HELLO_DELAY_MS", "60000".to_string()),
            ("FAKE_AFT_RECORD_SIGTERM", "1".to_string()),
            ("FAKE_AFT_EVENTS_PATH", path_string(&events)),
        ],
    );
    wait_for_event(&events, "hello_delay_started", SETUP_TIMEOUT).await;
    let running = wait_for_status(&module, SETUP_TIMEOUT, |status| {
        status.state == ModuleState::Running && status.pid.is_some()
    })
    .await;
    let pid = running.pid.unwrap();

    module.restart(None).await.unwrap();
    let exit = wait_for_terminal(&module, SETUP_TIMEOUT).await;
    assert_eq!(exit.exit_signal, Some(9), "{exit:?}");
    let line = assert_logged(
        module_id,
        "drain budget expired before the module exited; killing it",
        Duration::from_secs(1),
    )
    .await;
    assert!(line.contains("WARN"), "{line}");
    assert!(line.contains(&format!("pid={pid}")), "{line}");
    assert!(line.contains("budget_ms=1500"), "{line}");
    assert!(line.contains("reason=Restarting"), "{line}");
    wait_for_event(&events, "sigterm", Duration::from_secs(1)).await;

    module.stop().await.unwrap();
}

/// A module the supervisor is restarting can hold a registration the forwarding
/// table does not see as draining: here, the child that registered after its
/// drain had begun. A consumer reaching it must be told to retry soon
/// (`module_reloading`), not that the target is unavailable.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_open_against_a_module_mid_restart_is_refused_as_reloading() {
    let harness = Harness::start("drain-before-hello-reloading", Duration::from_secs(4)).await;
    let module_id = "mid-restart-route-open";
    let events = harness.events_path(module_id);
    // Ignores SIGTERM and registers 800 ms after starting, so it registers
    // while the drain that began before its HELLO is still waiting on it.
    let module = harness.spawn(
        module_id,
        &[
            ("FAKE_AFT_HELLO_DELAY_MS", "800".to_string()),
            ("FAKE_AFT_RECORD_SIGTERM", "1".to_string()),
            ("FAKE_AFT_EVENTS_PATH", path_string(&events)),
        ],
    );
    wait_for_event(&events, "hello_delay_started", SETUP_TIMEOUT).await;
    wait_for_status(&module, SETUP_TIMEOUT, |status| {
        status.state == ModuleState::Running && status.pid.is_some()
    })
    .await;

    module.restart(None).await.unwrap();
    wait_for_status(&module, SETUP_TIMEOUT, |status| {
        status.state == ModuleState::Draining && status.registration_active
    })
    .await;

    let project = TestTempDir::new("drain-before-hello-project");
    let mut client = connect_authed_client(&harness.daemon.connection_file_path)
        .await
        .unwrap();
    let error = route_open_error(&mut client, project.path(), 21, module_id).await;
    assert_eq!(
        error.code, "module_reloading",
        "a module mid-restart must be refused as retryable: {error:?}"
    );

    module.stop().await.unwrap();
}

async fn wait_for_status(
    module: &SupervisedModule,
    wait: Duration,
    matches: impl Fn(&ModuleStatus) -> bool,
) -> ModuleStatus {
    let deadline = Instant::now() + wait;
    loop {
        let status = module.status().unwrap();
        if matches(&status) {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "module {} did not reach the expected status within {wait:?}; last: {status:?}",
            module.module_id()
        );
        sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(unix)]
async fn wait_for_terminal(
    module: &SupervisedModule,
    wait: Duration,
) -> subc_daemon::terminal_ring::TerminalRecord {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(entry) = module.terminal_history().entries.first() {
            return entry.clone();
        }
        assert!(
            Instant::now() < deadline,
            "module {} recorded no exit within {wait:?}",
            module.module_id()
        );
        sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(unix)]
async fn wait_for_event(path: &Path, kind: &str, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        let found = std::fs::read_to_string(path).is_ok_and(|text| {
            text.lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .any(|event| event["kind"] == kind)
        });
        if found {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "stub event {kind} did not appear in {} within {wait:?}",
            path.display()
        );
        sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(unix)]
fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn control_frame(corr: u64, request: ClientControlRequest) -> Frame {
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        corr,
        serde_json::to_vec(&request).unwrap(),
    )
    .unwrap()
}

#[cfg(unix)]
async fn route_open_error(
    client: &mut tokio::net::TcpStream,
    project: &Path,
    corr: u64,
    module_id: &str,
) -> ErrorBody {
    let request = ClientControlRequest::RouteOpen {
        target: RouteTarget::ToolProvider {
            module_id: module_id.to_string(),
        },
        identity: BindIdentity::new(
            project.to_path_buf(),
            "opencode".to_string(),
            "ses-drain-before-hello".to_string(),
        ),
        consumer_identity: None,
        consumer_capabilities: None,
        admission_facts: None,
    };
    write_frame(client, &control_frame(corr, request))
        .await
        .unwrap();
    client.flush().await.unwrap();
    let frame = timeout(SETUP_TIMEOUT, read_frame(client))
        .await
        .expect("route.open answer timed out")
        .unwrap()
        .expect("connection closed before the route.open answer");
    assert_eq!(
        frame.header.ty,
        FrameType::Error,
        "route.open must be refused: {frame:?}"
    );
    assert_eq!(frame.header.corr, corr);
    serde_json::from_slice(&frame.body).unwrap()
}

/// Wait for a captured log line naming `module_id` and containing `needle`, and
/// return it.
async fn assert_logged(module_id: &str, needle: &str, wait: Duration) -> String {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(line) = log_capture()
            .lines()
            .into_iter()
            .find(|line| line.contains(module_id) && line.contains(needle))
        {
            return line;
        }
        assert!(
            Instant::now() < deadline,
            "no log line for {module_id} containing {needle:?}; captured lines for it: {:#?}",
            log_capture()
                .lines()
                .into_iter()
                .filter(|line| line.contains(module_id))
                .collect::<Vec<_>>()
        );
        sleep(Duration::from_millis(10)).await;
    }
}

/// Every log line this test process emits, from every thread.
///
/// Installed as the global default once: the supervisor logs from tasks on the
/// runtime's worker threads, which a thread-local subscriber would not see.
/// Tests running in parallel share it, so assertions filter by module id.
#[derive(Clone, Default)]
struct LogCapture {
    lines: Arc<Mutex<Vec<String>>>,
}

impl LogCapture {
    fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }
}

fn log_capture() -> &'static LogCapture {
    static CAPTURE: OnceLock<LogCapture> = OnceLock::new();
    CAPTURE.get_or_init(|| {
        let capture = LogCapture::default();
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_max_level(tracing::Level::INFO)
                .with_writer(capture.clone())
                .finish(),
        )
        .expect("this test binary installs one global tracing subscriber");
        capture
    })
}

impl<'a> MakeWriter<'a> for LogCapture {
    type Writer = LineWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LineWriter {
            lines: Arc::clone(&self.lines),
            buf: Vec::new(),
        }
    }
}

/// Buffers one formatted event and stores it as a line when dropped, which is
/// when the fmt layer has finished writing that event.
struct LineWriter {
    lines: Arc<Mutex<Vec<String>>>,
    buf: Vec<u8>,
}

impl std::io::Write for LineWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for LineWriter {
    fn drop(&mut self) {
        let line = String::from_utf8_lossy(&self.buf).trim().to_owned();
        if !line.is_empty() {
            self.lines.lock().unwrap().push(line);
        }
    }
}
