use super::*;
use crate::{
    control::ControlHandler,
    operator_apps::{OperatorAppPolicy, TestOperatorAppPolicy},
    registry::ConnectionId,
    router::{FrameSink, RouteCtx},
};
use serde_json::{json, Value};
use subc_protocol::{Flags, FrameType, Priority};

const TEST_APP: &str = "test-app";

fn test_policy() -> TestOperatorAppPolicy {
    TestOperatorAppPolicy {
        macos_enabled: false,
        windows_enabled: true,
        linux_enabled: true,
        app: TEST_APP,
        requirement: "always",
        floor: Version::new(1, 2, 10),
        windows_publisher: Some("test-publisher"),
        windows_path: None,
        linux_path: None,
        windows_breakaway: true,
        linux_force_direct: true,
    }
}

fn test_handler() -> ControlHandler {
    ControlHandler::default().with_test_operator_app_policy(test_policy())
}

// Workspace builds turn on serde_json's preserve_order through another crate,
// so a Value's key order differs between `-p subc-daemon` and workspace test
// runs. Sort keys explicitly so the sample bytes are the same in both.
fn sorted_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, sorted_keys(value)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted_keys).collect()),
        other => other,
    }
}

#[tokio::test]
async fn app_open_vectors_match_real_handler_bytes() {
    use sha2::{Digest, Sha256};

    const PID: u32 = 4242;
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vectors/app-open-v1");
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let mut sums = String::new();
    for case in [
        "already_running",
        "app_not_installed",
        "app_open_disabled",
        "app_open_not_used",
        "app_unknown",
        "app_verification_failed",
        "launched",
    ] {
        let mut policy = test_policy();
        if case == "app_open_disabled" {
            policy.linux_enabled = false;
        }
        if matches!(case, "launched" | "app_verification_failed") {
            // Only existence is checked before the injected launcher. The path
            // never reaches a frame, and no executable is run by this test.
            policy.linux_path = Some(std::env::current_exe().unwrap());
        }
        let mut handler = ControlHandler::default().with_test_operator_app_policy(policy);
        handler.app_open_test_platform = Some(if case == "app_open_not_used" {
            AppOpenPlatform::MacOS
        } else {
            AppOpenPlatform::Linux
        });
        handler.app_open_test_launch = Some(if case == "app_verification_failed" {
            || {
                Err(AppOpenRefusal {
                    code: subc_control::APP_VERIFICATION_FAILED,
                    message: "installed app failed verification".to_string(),
                })
            }
        } else {
            || Ok(PID)
        });
        if case == "already_running" {
            handler
                .launch_table
                .publish(TEST_APP, PID, live_process(), None);
        }
        let app = if case == "app_unknown" {
            "unknown-app"
        } else {
            TEST_APP
        };
        let request = app_open_frame(11, app);
        let (ctx, _rx) = route_ctx(7);
        let replies = handler
            .handle_control_frame(&ctx, request.clone())
            .await
            .unwrap();
        assert_eq!(replies.len(), 1);
        let reply: Value = serde_json::from_slice(&replies[0].body).unwrap();
        let request: Value = serde_json::from_slice(&request.body).unwrap();
        let sample = match replies[0].header.ty {
            FrameType::Response => {
                assert_eq!(reply["status"], case);
                assert_eq!(reply["pid"], PID);
                json!({"request":request, "reply":reply})
            }
            FrameType::Error => {
                assert_eq!(reply["code"], case);
                assert_eq!(reply.as_object().unwrap().len(), 2);
                json!({"request":request, "error":reply})
            }
            ty => panic!("unexpected app.open reply: {ty:?}"),
        };
        let mut bytes = serde_json::to_vec_pretty(&sorted_keys(sample)).unwrap();
        bytes.push(b'\n');
        let name = format!("{case}.json");
        if update {
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join(&name), &bytes).unwrap();
        }
        assert_eq!(
            std::fs::read(directory.join(&name)).unwrap(),
            bytes,
            "{name}; regenerate with UPDATE_GOLDEN=1"
        );
        sums.push_str(&format!("{:x}  {name}\n", Sha256::digest(&bytes)));
    }
    if update {
        std::fs::write(directory.join("SHA256SUMS"), &sums).unwrap();
    }
    assert_eq!(
        std::fs::read(directory.join("SHA256SUMS")).unwrap(),
        sums.as_bytes()
    );
}

fn route_ctx(
    connection: u64,
) -> (
    RouteCtx,
    tokio::sync::mpsc::Receiver<crate::router::OutboundFrame>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    (
        RouteCtx {
            connection_id: ConnectionId::new(connection),
            egress: FrameSink::new(tx),
        },
        rx,
    )
}

fn app_open_frame(corr: u64, app: &str) -> Frame {
    let body = serde_json::to_vec(&subc_control::AppOpenRequest::new(app)).unwrap();
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        corr,
        body,
    )
    .unwrap()
}

async fn call_app_open(handler: &ControlHandler, app: &str) -> Vec<Frame> {
    let (ctx, _rx) = route_ctx(7);
    handler
        .handle_control_frame(&ctx, app_open_frame(11, app))
        .await
        .unwrap()
}

fn error_code(frame: &Frame) -> (String, String) {
    assert_eq!(frame.header.ty, FrameType::Error);
    let body: Value = serde_json::from_slice(&frame.body).unwrap();
    (
        body["code"].as_str().unwrap().to_owned(),
        body["message"].as_str().unwrap_or_default().to_owned(),
    )
}

#[cfg(any(target_os = "linux", windows))]
fn response_body(frame: &Frame) -> Value {
    assert_eq!(
        frame.header.ty,
        FrameType::Response,
        "expected a response, got {:?}: {}",
        frame.header.ty,
        String::from_utf8_lossy(&frame.body)
    );
    serde_json::from_slice(&frame.body).unwrap()
}

/// A live handle on this test process: alive on every platform, so decisions
/// and tables can be driven without spawning anything.
fn live_process() -> subc_os::Process {
    subc_os::Process::open(std::process::id())
        .unwrap()
        .expect("the test process is alive")
}

fn publish_self(table: &LaunchTable, app: &str) -> u64 {
    table.publish(app, std::process::id(), live_process(), None)
}

fn decide(policy: &OperatorAppPolicy, platform: AppOpenPlatform, app: &str) -> AppOpenDecision {
    decide_app_open(policy, &LaunchTable::new(), platform, app)
}

fn decide_code(decision: AppOpenDecision) -> &'static str {
    match decision {
        AppOpenDecision::Refuse(refusal) => refusal.code,
        AppOpenDecision::AlreadyRunning { .. } => "already_running",
        AppOpenDecision::Launch { .. } => "launched",
    }
}

#[test]
fn decision_order_macos_and_other_never_launch() {
    let policy = OperatorAppPolicy::from(test_policy());
    let table = LaunchTable::new();
    // macOS answers `app_open_not_used` with the switch on and off, and for
    // an unknown app: it is the first check.
    for enabled in [false, true] {
        let mut policy = policy.clone();
        policy.macos_enabled = enabled;
        for app in [TEST_APP, "unknown"] {
            assert_eq!(
                decide_code(decide_app_open(
                    &policy,
                    &table,
                    AppOpenPlatform::MacOS,
                    app
                )),
                subc_control::APP_OPEN_NOT_USED,
                "enabled={enabled} app={app}"
            );
        }
    }
    assert_eq!(
        decide_code(decide(&policy, AppOpenPlatform::Other, TEST_APP)),
        subc_control::APP_OPEN_NOT_USED
    );
    // A macOS `app.open` over the wire is refused before any table or
    // filesystem state matters.
    let _ = table;
}

#[test]
fn decision_order_disabled_before_unknown() {
    let mut policy = test_policy();
    policy.windows_enabled = false;
    policy.linux_enabled = false;
    let policy = OperatorAppPolicy::from(policy);
    for platform in [AppOpenPlatform::Windows, AppOpenPlatform::Linux] {
        // Unknown app with the switch off: the switch is checked first.
        assert_eq!(
            decide_code(decide(&policy, platform, "unknown")),
            subc_control::APP_OPEN_DISABLED,
            "{platform:?}"
        );
        assert_eq!(
            decide_code(decide(&policy, platform, TEST_APP)),
            subc_control::APP_OPEN_DISABLED,
            "{platform:?}"
        );
    }
}

#[test]
fn decision_order_non_test_table_answers_unknown() {
    // The production table names no app and no install location. With a
    // platform switch forced on, `app.open` answers `app_unknown` before any
    // filesystem or signature check runs; with the switch off the disabled
    // check fires first, per the fixed order.
    let policy = OperatorAppPolicy::default();
    assert!(policy.app_id().is_none());
    for platform in [AppOpenPlatform::Windows, AppOpenPlatform::Linux] {
        assert_eq!(
            decide_code(decide(&policy, platform, TEST_APP)),
            subc_control::APP_OPEN_DISABLED,
            "{platform:?}: the switch check precedes the table check"
        );
    }
    let mut switched = OperatorAppPolicy::default();
    switched.windows_enabled = true;
    switched.linux_enabled = true;
    for platform in [AppOpenPlatform::Windows, AppOpenPlatform::Linux] {
        assert_eq!(
            decide_code(decide(&switched, platform, TEST_APP)),
            subc_control::APP_UNKNOWN,
            "{platform:?}"
        );
        assert_eq!(
            decide_code(decide(&switched, platform, "unknown")),
            subc_control::APP_UNKNOWN,
            "{platform:?}"
        );
    }
}

#[test]
fn decision_order_already_running_beats_not_installed() {
    // A live entry answers `already_running` even when the installed file is
    // gone: the running process is what the caller asked about. The policy
    // here names no install location at all, which would otherwise be
    // `app_not_installed`.
    let mut policy = test_policy();
    policy.windows_path = None;
    policy.linux_path = None;
    let policy = OperatorAppPolicy::from(policy);
    for platform in [AppOpenPlatform::Windows, AppOpenPlatform::Linux] {
        let table = LaunchTable::new();
        assert_eq!(
            decide_code(decide_app_open(&policy, &table, platform, TEST_APP)),
            subc_control::APP_NOT_INSTALLED,
            "{platform:?} without an entry"
        );
        let incarnation = publish_self(&table, TEST_APP);
        assert!(table.is_live_incarnation(TEST_APP, incarnation));
        match decide_app_open(&policy, &table, platform, TEST_APP) {
            AppOpenDecision::AlreadyRunning { pid } => {
                assert_eq!(pid, std::process::id());
            }
            other => panic!(
                "{platform:?}: expected already_running, got {}",
                decide_code(other)
            ),
        }
    }
}

#[test]
fn decision_order_unknown_then_not_installed_then_launch() {
    let policy = OperatorAppPolicy::from(test_policy());
    for platform in [AppOpenPlatform::Windows, AppOpenPlatform::Linux] {
        let table = LaunchTable::new();
        assert_eq!(
            decide_code(decide_app_open(&policy, &table, platform, "unknown")),
            subc_control::APP_UNKNOWN,
            "{platform:?}"
        );
        assert_eq!(
            decide_code(decide_app_open(&policy, &table, platform, TEST_APP)),
            subc_control::APP_NOT_INSTALLED,
            "{platform:?} with no install location"
        );
        // An existing file advances the decision to the launch step.
        let dir = cortexkit_test_support::ScratchDir::new("app-open-decide");
        let installed = dir.join("installed");
        std::fs::write(&installed, b"installed").unwrap();
        let mut policy = test_policy();
        if platform == AppOpenPlatform::Windows {
            policy.windows_path = Some(installed.clone());
        } else {
            policy.linux_path = Some(installed.clone());
        }
        let policy = OperatorAppPolicy::from(policy);
        match decide_app_open(&policy, &table, platform, TEST_APP) {
            AppOpenDecision::Launch { path } => assert_eq!(path, installed),
            other => panic!("{platform:?}: expected launch, got {}", decide_code(other)),
        }
    }
}

#[test]
fn launch_table_retires_dead_entries_and_reuses_nothing() {
    let table = LaunchTable::new();
    assert_eq!(table.live_entry(TEST_APP), None);
    assert_eq!(table.match_live_pid(TEST_APP, 1), None);
    assert!(!table.is_live_incarnation(TEST_APP, 1));

    let first = publish_self(&table, TEST_APP);
    let live = table.live_entry(TEST_APP).expect("just published");
    assert_eq!(live.pid, std::process::id());
    assert_eq!(live.incarnation, first);
    assert_eq!(table.match_live_pid(TEST_APP, live.pid), Some(first));
    assert_eq!(
        table.match_live_pid(TEST_APP, live.pid.wrapping_add(1)),
        None
    );
    assert!(table.is_live_incarnation(TEST_APP, first));
    assert!(!table.is_live_incarnation(TEST_APP, first.wrapping_add(1)));

    // Retirement closes the entry: the same numeric pid never matches again,
    // and the old incarnation never revalidates.
    table.retire(TEST_APP);
    assert_eq!(table.live_entry(TEST_APP), None);
    assert_eq!(table.match_live_pid(TEST_APP, live.pid), None);
    assert!(!table.is_live_incarnation(TEST_APP, first));

    // A replacement gets a fresh incarnation; the old one stays dead.
    let second = publish_self(&table, TEST_APP);
    assert_ne!(second, first);
    assert!(table.is_live_incarnation(TEST_APP, second));
    assert!(!table.is_live_incarnation(TEST_APP, first));
    // Publishing over a live entry retires it first: no entry leaks.
    let third = publish_self(&table, TEST_APP);
    assert!(!table.is_live_incarnation(TEST_APP, second));
    assert!(table.is_live_incarnation(TEST_APP, third));
}

#[test]
fn launch_table_reports_exit_as_dead() {
    // Publish a live child, end it, and require the table to read the exit
    // from its handle: the numeric pid alone must never read as live.
    #[cfg(unix)]
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    #[cfg(windows)]
    let mut child = std::process::Command::new("C:\\Windows\\System32\\cmd.exe")
        .args(["/c", "ping -n 30 127.0.0.1 >NUL"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    #[cfg(not(any(unix, windows)))]
    let mut child = {
        let _ = TEST_APP;
        unreachable!("this test runs on unix or Windows");
    };
    let pid = child.id();
    let table = LaunchTable::new();
    let process = subc_os::Process::open(pid).unwrap().unwrap();
    let incarnation = table.publish(TEST_APP, pid, process, None);
    assert!(table.is_live_incarnation(TEST_APP, incarnation));
    child.kill().unwrap();
    let _ = child.wait();
    // The handle reports the exit even though no wait on our side reaped
    // through the entry (the direct `Child` above is outside the table).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while table.live_entry(TEST_APP).is_some() {
        assert!(
            std::time::Instant::now() < deadline,
            "exited child still reads as live"
        );
        std::thread::yield_now();
    }
    assert!(!table.is_live_incarnation(TEST_APP, incarnation));
}

#[tokio::test]
async fn malformed_app_open_body_is_refused_before_any_check() {
    // No `app` field: the body cannot launch anything, whatever the platform.
    let (ctx, _rx) = route_ctx(7);
    let body = serde_json::to_vec(&json!({"op": "app.open"})).unwrap();
    let frame = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        11,
        body,
    )
    .unwrap();
    let replies = test_handler()
        .handle_control_frame(&ctx, frame)
        .await
        .unwrap();
    assert_eq!(replies.len(), 1);
    let (code, _) = error_code(&replies[0]);
    assert_eq!(code, "invalid_control_body");
}

#[tokio::test]
async fn enforce_mode_never_gates_app_open() {
    // `app.open` is classified `None` in the privileged-operation classifier:
    // even in `enforce` mode no prompt, decline or dispatch machinery engages.
    // The call answers with the platform's own code at once.
    #[cfg(windows)]
    let policy = {
        let mut policy = test_policy();
        // Reach the launch step, where the fail-closed default (no verifier
        // in a plain test build) refuses without prompting.
        policy.windows_path = Some(std::env::current_exe().unwrap());
        policy
    };
    #[cfg(not(windows))]
    let policy = test_policy();
    let handler = ControlHandler::default()
        .with_test_operator_app_policy(policy)
        .with_privileged_operator_prompt(crate::daemon_config::PrivilegedOperatorPrompt::Enforce);
    let replies = call_app_open(&handler, TEST_APP).await;
    assert_eq!(replies.len(), 1);
    match AppOpenPlatform::current() {
        AppOpenPlatform::MacOS | AppOpenPlatform::Other => {
            let (code, _) = error_code(&replies[0]);
            assert_eq!(code, subc_control::APP_OPEN_NOT_USED);
        }
        AppOpenPlatform::Linux => {
            // Force-direct with no install location: refused without a prompt.
            let (code, _) = error_code(&replies[0]);
            assert_eq!(code, subc_control::APP_NOT_INSTALLED);
        }
        AppOpenPlatform::Windows => {
            // No verifier in the test build: refused without a prompt.
            let (code, _) = error_code(&replies[0]);
            assert_eq!(code, subc_control::APP_VERIFICATION_FAILED);
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn launch_style_selection_prefers_a_scope_only_with_a_manager() {
    assert_eq!(
        select_linux_launch_style(true, true),
        LinuxLaunchStyle::Scope
    );
    for (systemd_run, bus) in [(true, false), (false, true), (false, false)] {
        assert_eq!(
            select_linux_launch_style(systemd_run, bus),
            LinuxLaunchStyle::Direct,
            "systemd_run={systemd_run} bus={bus}"
        );
    }
}
#[cfg(target_os = "linux")]
mod linux_tests {
    use super::*;
    use subc_os::local_peer::{ObservedPeer as OsPeer, PeerObservation, PeerUser};

    fn linux_policy(path: PathBuf) -> TestOperatorAppPolicy {
        TestOperatorAppPolicy {
            macos_enabled: false,
            windows_enabled: false,
            linux_enabled: true,
            app: TEST_APP,
            requirement: "always",
            floor: Version::new(1, 2, 10),
            windows_publisher: None,
            windows_path: None,
            linux_path: Some(path),
            windows_breakaway: true,
            linux_force_direct: true,
        }
    }

    fn launched_pid(frame: &Frame) -> u32 {
        let body = response_body(frame);
        assert_eq!(
            body["status"].as_str(),
            Some("launched"),
            "expected a launch reply, got {body}"
        );
        body["pid"].as_u64().expect("launch pid") as u32
    }

    fn kill(pid: u32) {
        if let Some(process) = subc_os::Process::open(pid).unwrap() {
            let _ = process.signal(subc_os::Signal::Kill);
        }
    }

    /// A genuinely long-lived installed file: `app.open` passes no
    /// arguments, and bare `/bin/sleep` exits at once with a usage error,
    /// so the fixture is a script that sleeps. The script content is
    /// static; only its path varies.
    fn sleeper(dir: &cortexkit_test_support::ScratchDir) -> PathBuf {
        let path = dir.join("sleeper.sh");
        std::fs::write(&path, "#!/bin/sh\nexec /bin/sleep 30\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// Wait until the entry retires, so a directly spawned child is reaped
    /// through its retained handle instead of lingering as a zombie.
    async fn wait_for_retire(handler: &ControlHandler) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while handler.launch_table.live_entry(TEST_APP).is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "killed app still reads as live"
            );
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn linux_direct_launch_reports_already_running_then_relaunches() {
        let dir = cortexkit_test_support::ScratchDir::new("app-open-relaunch");
        let handler =
            ControlHandler::default().with_test_operator_app_policy(linux_policy(sleeper(&dir)));
        assert!(Path::new("/bin/sleep").exists());
        let first = launched_pid(&call_app_open(&handler, TEST_APP).await[0]);
        let live = handler
            .launch_table
            .live_entry(TEST_APP)
            .expect("a launch entry is published before the reply");
        assert_eq!(live.pid, first);
        let first_incarnation = live.incarnation;

        // While the process lives, no second process starts: the same pid and
        // the same incarnation answer `already_running`.
        let replies = call_app_open(&handler, TEST_APP).await;
        let body = response_body(&replies[0]);
        assert_eq!(body["status"].as_str(), Some("already_running"));
        assert_eq!(body["pid"].as_u64(), Some(u64::from(first)));
        assert!(handler
            .launch_table
            .is_live_incarnation(TEST_APP, first_incarnation));

        // After the exit the entry retires, and the next call launches a new
        // process with a fresh incarnation: the old pid confers nothing.
        kill(first);
        wait_for_retire(&handler).await;
        assert!(!handler
            .launch_table
            .is_live_incarnation(TEST_APP, first_incarnation));
        let second = launched_pid(&call_app_open(&handler, TEST_APP).await[0]);
        let live = handler.launch_table.live_entry(TEST_APP).unwrap();
        assert_eq!(live.pid, second);
        assert_ne!(live.incarnation, first_incarnation);
        kill(second);
        wait_for_retire(&handler).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn linux_concurrent_app_open_launches_once_then_reports_already_running() {
        // Two calls racing the same app serialize on the handler's launch
        // lock: exactly one launch happens, and both answers name its pid.
        let dir = cortexkit_test_support::ScratchDir::new("app-open-race");
        let installed = sleeper(&dir);
        let handler = std::sync::Arc::new(
            ControlHandler::default().with_test_operator_app_policy(linux_policy(installed)),
        );
        let start = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let first_gate = std::sync::Arc::clone(&start);
        let second_gate = std::sync::Arc::clone(&start);
        // One `Arc` per racer: the tables and the launch lock behind them
        // are shared, which is exactly what the test synchronizes on.
        let first_handler = std::sync::Arc::clone(&handler);
        let second_handler = std::sync::Arc::clone(&handler);
        let (first, second) = tokio::join!(
            async move {
                first_gate.wait().await;
                call_app_open(&first_handler, TEST_APP).await
            },
            async move {
                second_gate.wait().await;
                call_app_open(&second_handler, TEST_APP).await
            }
        );
        let mut launched = None;
        let mut running = None;
        for replies in [&first, &second] {
            assert_eq!(replies.len(), 1);
            let body = response_body(&replies[0]);
            match body["status"].as_str() {
                Some("launched") => launched = Some(body["pid"].as_u64().expect("launch pid")),
                Some("already_running") => {
                    running = Some(body["pid"].as_u64().expect("running pid"))
                }
                other => panic!("unexpected app.open answer: {other:?}"),
            }
        }
        let (launched, running) = (
            launched.expect("one call launched"),
            running.expect("one call found it running"),
        );
        assert_eq!(
            launched, running,
            "both answers name the single launched process"
        );
        let live = handler
            .launch_table
            .live_entry(TEST_APP)
            .expect("one live entry");
        assert_eq!(u64::from(live.pid), launched);
        kill(launched as u32);
        wait_for_retire(&handler).await;
    }

    #[tokio::test]
    async fn linux_direct_launch_scrubs_loader_overrides_but_keeps_the_environment() {
        // The launch environment inherits everything except the loader
        // overrides. The poison rides the launch command itself, never the
        // test process environment, so parallel tests spawning children
        // cannot observe it. The values name files that do not exist, so a
        // scrub regression fails the launch loudly instead of weakening the
        // assertion. Only command-provided variables are asserted: ambient
        // variables like `PATH` are off limits, because parallel tests may
        // unset them globally at any moment.
        let dir = cortexkit_test_support::ScratchDir::new("app-open-env");
        let dump = dir.join("env.dump");
        // Unique variable names, so setting them cannot disturb parallel
        // tests; the dump path rides the command too, so the test touches
        // no process-global state at all.
        let dump_key = "SUBC_APP_OPEN_ENV_DUMP";
        let dump_value = dump.display().to_string();
        let marker_key = "SUBC_APP_OPEN_INHERITED";
        let probe = dir.join("probe.sh");
        // Absolute tool paths throughout: the script must not depend on the
        // test process's `PATH`, which parallel tests are allowed to mutate.
        // (`/usr/bin/env` is where coreutils installs it on Linux.) The
        // trailing sentinel proves the dump is complete: the test polls for
        // it instead of trusting the file's mere existence, which
        // redirection creates before `env` finishes writing.
        std::fs::write(
            &probe,
            "#!/bin/sh\n/usr/bin/env > \"$SUBC_APP_OPEN_ENV_DUMP\"\necho done >> \"$SUBC_APP_OPEN_ENV_DUMP\"\nexec /bin/sleep 30\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let handler =
            ControlHandler::default().with_test_operator_app_policy(linux_policy(probe.clone()));
        let pid = handler
            .linux_launch_direct(
                TEST_APP,
                &probe,
                &[
                    ("LD_PRELOAD", "/nonexistent-subc-preload.so"),
                    ("LD_LIBRARY_PATH", "/nonexistent-subc-lib"),
                    ("LD_AUDIT", "/nonexistent-subc-audit.so"),
                    (dump_key, dump_value.as_str()),
                    (marker_key, "inherited"),
                ],
            )
            .expect("the poisoned launch still starts");

        // The child writes the dump after the spawn call returns, so wait
        // for the sentinel instead of assuming it is already there: the
        // shell creates the file before `env` finishes writing it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let dumped = loop {
            if let Ok(dumped) = std::fs::read_to_string(&dump) {
                if dumped.lines().any(|line| line == "done") {
                    break dumped;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the probe never dumped its environment"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        for key in ["LD_PRELOAD", "LD_LIBRARY_PATH", "LD_AUDIT"] {
            assert!(
                !dumped.lines().any(|line| line.starts_with(key)),
                "{key} reached the app: scrub failed"
            );
        }
        // The command's environment reaches the child intact: anything the
        // command sets arrives, so the scrub removes only the three loader
        // overrides instead of clearing the environment.
        for (key, value) in [(dump_key, dump_value.as_str()), (marker_key, "inherited")] {
            assert!(
                dumped.lines().any(|line| line == format!("{key}={value}")),
                "the probe did not inherit the launch environment"
            );
        }
        kill(pid);
        wait_for_retire(&handler).await;
    }

    #[tokio::test]
    async fn linux_launch_records_but_never_stamps() {
        // The launch entry is recorded, and no connection from the launched
        // process is ever stamped `operator_app`.
        let dir = cortexkit_test_support::ScratchDir::new("app-open-nostamp");
        let installed = sleeper(&dir);
        let handler = ControlHandler::default()
            .with_test_operator_app_policy(linux_policy(installed.clone()));
        let pid = launched_pid(&call_app_open(&handler, TEST_APP).await[0]);
        assert!(handler.launch_table.live_entry(TEST_APP).is_some());
        let (ctx, _rx) = route_ctx(21);
        handler.record_captured_peer(
            ctx.connection_id,
            subc_os::local_peer::CapturedPeer::from(OsPeer::Observed(PeerObservation {
                user: PeerUser::Uid(rustix::process::geteuid().as_raw()),
                pid,
                exe: Some(installed),
                signing_id: None,
            })),
        );
        assert!(
            handler
                .operator_apps
                .lock()
                .unwrap()
                .get(&ctx.connection_id)
                .is_none(),
            "a Linux launch entry never stamps"
        );
        kill(pid);
        wait_for_retire(&handler).await;
    }

    #[tokio::test]
    async fn linux_restart_drops_entries_and_reconnects_unstamped() {
        // Installed-service restart at the handler level: launch entries die
        // with the daemon because they are memory-only. The launched process
        // survives outside the daemon's kill domain, and its new connection
        // is plain `direct` with no stamp.
        let dir = cortexkit_test_support::ScratchDir::new("app-open-restart");
        let installed = sleeper(&dir);
        let handler = ControlHandler::default()
            .with_test_operator_app_policy(linux_policy(installed.clone()));
        let pid = launched_pid(&call_app_open(&handler, TEST_APP).await[0]);
        drop(handler);

        let process = subc_os::Process::open(pid)
            .unwrap()
            .expect("the launched process survives the restart");
        assert!(
            process.observe().is_some(),
            "the launched process is still running"
        );
        let restarted = ControlHandler::default()
            .with_test_operator_app_policy(linux_policy(installed.clone()));
        assert!(
            restarted.launch_table.live_entry(TEST_APP).is_none(),
            "a restarted daemon tracks no launch"
        );
        let (ctx, _rx) = route_ctx(23);
        restarted.record_captured_peer(
            ctx.connection_id,
            subc_os::local_peer::CapturedPeer::from(OsPeer::Observed(PeerObservation {
                user: PeerUser::Uid(rustix::process::geteuid().as_raw()),
                pid,
                exe: Some(installed),
                signing_id: None,
            })),
        );
        assert!(
            restarted
                .operator_apps
                .lock()
                .unwrap()
                .get(&ctx.connection_id)
                .is_none(),
            "a post-restart connection is unstamped"
        );
        kill(pid);
    }

    #[test]
    fn systemd_run_command_starts_a_scope_for_the_installed_file() {
        // The scope wrapper's shape is pinned without needing a user manager:
        // `systemd-run` starts the scope and the wrapper reports the app's
        // pid for the launch entry.
        let command = systemd_run_command(Path::new("/opt/alfonso/alfonso"));
        let std_command = command.as_std();
        assert_eq!(std_command.get_program(), "systemd-run");
        let args: Vec<String> = std_command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(&args[..5], ["--user", "--scope", "--", "/bin/sh", "-c"]);
        assert!(
            args.iter().any(|arg| arg.contains("/opt/alfonso/alfonso")),
            "the installed path reaches the scope wrapper: {args:?}"
        );
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_app_open_is_never_used() {
    // macOS answers `app_open_not_used` with the switch on and off, for a
    // known app with an install location and for an unknown app: launching
    // through the daemon is not how macOS apps start.
    let mut policy = test_policy();
    policy.macos_enabled = true;
    policy.linux_path = Some(std::env::current_exe().unwrap());
    let handler = ControlHandler::default().with_test_operator_app_policy(policy);
    for app in [TEST_APP, "unknown"] {
        let replies = call_app_open(&handler, app).await;
        assert_eq!(replies.len(), 1);
        let (code, _) = error_code(&replies[0]);
        assert_eq!(code, subc_control::APP_OPEN_NOT_USED, "app={app}");
    }
}
#[cfg(windows)]
mod windows_tests {
    use super::*;
    use subc_os::local_peer::{ObservedPeer as OsPeer, PeerObservation, PeerUser};

    fn windows_policy(path: PathBuf) -> TestOperatorAppPolicy {
        TestOperatorAppPolicy {
            macos_enabled: false,
            windows_enabled: true,
            linux_enabled: false,
            app: TEST_APP,
            requirement: "always",
            floor: Version::new(1, 2, 10),
            windows_publisher: Some("test-publisher"),
            windows_path: Some(path),
            linux_path: None,
            windows_breakaway: false,
            linux_force_direct: false,
        }
    }

    /// The no-arguments long-lived fixture: a GUI app that stays running with
    /// null stdio, exactly as `app.open` launches it (the wire carries no
    /// arguments).
    fn notepad() -> PathBuf {
        let system_root = std::env::var("SystemRoot").expect("SystemRoot is always set");
        let path = PathBuf::from(system_root)
            .join("System32")
            .join("notepad.exe");
        assert!(
            path.exists(),
            "notepad.exe is the no-args long-lived fixture: {}",
            path.display()
        );
        path
    }

    fn copy_test_exe(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("subc-app-open-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let copy = dir.join("app.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &copy).unwrap();
        copy
    }

    /// An accepting check that also proves what it ran on: the test publisher
    /// and floor reach the check, and the installed file is still pinned
    /// while held, because renaming it fails.
    fn accept_verifier(
        seen: std::sync::Arc<std::sync::Mutex<Vec<WindowsVerifyRequest>>>,
        version: &'static str,
    ) -> WindowsVerifyFn {
        std::sync::Arc::new(
            move |_: &subc_os::ExecutableCapture, request: &WindowsVerifyRequest| {
                assert_eq!(request.publisher.as_deref(), Some("test-publisher"));
                assert_eq!(request.floor, Version::new(1, 2, 10));
                assert!(
                    std::fs::rename(&request.path, request.path.with_extension("renamed")).is_err(),
                    "the held handle must pin the installed file during verification"
                );
                seen.lock().unwrap().push(request.clone());
                Ok(version.to_owned())
            },
        )
    }

    fn spawn_attempts(handler: &ControlHandler) -> u64 {
        handler
            .windows_spawn_attempts
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn stop_windows_process(pid: u32) {
        if let Some(process) = subc_os::Process::open(pid).unwrap() {
            if let Some(observed) = process.observe() {
                let _ = process.force_stop(observed.start_time, std::time::Duration::from_secs(10));
            }
        }
    }

    async fn wait_for_retire(handler: &ControlHandler) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while handler.launch_table.live_entry(TEST_APP).is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "stopped app still reads as live"
            );
            tokio::task::yield_now().await;
        }
    }

    fn launched_pid_of(frame: &Frame) -> u32 {
        let body = response_body(frame);
        assert_eq!(
            body["status"].as_str(),
            Some("launched"),
            "expected a launch reply, got {body}"
        );
        body["pid"].as_u64().expect("launch pid") as u32
    }

    #[tokio::test]
    async fn windows_launch_verifies_resumes_tracks_flashes_and_relaunches() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let flashed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let flashed_hook = std::sync::Arc::clone(&flashed);
        let handler = ControlHandler::default()
            .with_test_operator_app_policy(windows_policy(notepad()))
            .with_test_windows_verifier(accept_verifier(std::sync::Arc::clone(&seen), "1.2.10"))
            .with_test_windows_flash(std::sync::Arc::new(move |pid| {
                flashed_hook.lock().unwrap().push(pid);
            }));

        // The test hook receives the held capture with the expected publisher
        // and floor and returns a version; the child starts suspended outside
        // the daemon's root job, its image compares, and it resumes: the entry
        // is published before the image runs.
        let first = launched_pid_of(&call_app_open(&handler, TEST_APP).await[0]);
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(spawn_attempts(&handler), 1);
        let live = handler
            .launch_table
            .live_entry(TEST_APP)
            .expect("the entry is published before the image runs");
        assert_eq!(live.pid, first);
        let first_incarnation = live.incarnation;

        // While it lives, `app.open` starts nothing and only flashes: the
        // spawn count is unchanged and the flash hook saw the pid.
        let replies = call_app_open(&handler, TEST_APP).await;
        let body = response_body(&replies[0]);
        assert_eq!(body["status"].as_str(), Some("already_running"));
        assert_eq!(body["pid"].as_u64(), Some(u64::from(first)));
        assert_eq!(spawn_attempts(&handler), 1);
        assert_eq!(*flashed.lock().unwrap(), vec![first]);

        // After the exit the entry retires, and the next call re-verifies the
        // current bytes and launches a new process with a fresh incarnation.
        stop_windows_process(first);
        wait_for_retire(&handler).await;
        assert!(!handler
            .launch_table
            .is_live_incarnation(TEST_APP, first_incarnation));
        let second = launched_pid_of(&call_app_open(&handler, TEST_APP).await[0]);
        let live = handler.launch_table.live_entry(TEST_APP).unwrap();
        assert_eq!(live.pid, second);
        assert_ne!(live.incarnation, first_incarnation);
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert_eq!(spawn_attempts(&handler), 2);
        stop_windows_process(second);
        wait_for_retire(&handler).await;
    }

    #[tokio::test]
    async fn windows_rejections_start_nothing_and_stamp_nothing() {
        // Simulated verifier refusals: a wrong publisher, an unsigned binary,
        // a correctly signed binary below the floor, and a malformed version
        // are all refused with `app_verification_failed`. No process starts
        // and nothing is stamped. The below-floor version is numerically
        // lower but lexically higher than the floor, so the check is semver
        // ordering, not string comparison.
        let copy = copy_test_exe("reject");
        for (label, answer) in [
            ("wrong-publisher", Err("untrusted publisher".to_owned())),
            ("unsigned", Err("no signature".to_owned())),
            ("below-floor", Ok("1.2.9".to_owned())),
            ("malformed", Ok("banana".to_owned())),
        ] {
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let seen_hook = std::sync::Arc::clone(&seen);
            let verify: WindowsVerifyFn = std::sync::Arc::new(
                move |_: &subc_os::ExecutableCapture, request: &WindowsVerifyRequest| {
                    assert_eq!(request.publisher.as_deref(), Some("test-publisher"));
                    assert_eq!(request.floor, Version::new(1, 2, 10));
                    seen_hook.lock().unwrap().push(request.clone());
                    answer.clone()
                },
            );
            let handler = ControlHandler::default()
                .with_test_operator_app_policy(windows_policy(copy.clone()))
                .with_test_windows_verifier(verify);
            let replies = call_app_open(&handler, TEST_APP).await;
            assert_eq!(replies.len(), 1);
            let (code, _) = error_code(&replies[0]);
            assert_eq!(code, subc_control::APP_VERIFICATION_FAILED, "{label}");
            assert_eq!(seen.lock().unwrap().len(), 1, "{label}: verification ran");
            assert_eq!(spawn_attempts(&handler), 0, "{label}: no process may start");
            assert!(
                handler.launch_table.live_entry(TEST_APP).is_none(),
                "{label}: no entry is published"
            );
            assert!(
                handler.operator_apps.lock().unwrap().is_empty(),
                "{label}: nothing is stamped"
            );
        }
        let _ = std::fs::remove_dir_all(copy.parent().unwrap());
    }

    #[tokio::test]
    async fn windows_untrackable_launch_is_stopped_and_publishes_nothing() {
        // A launch whose tracking handle cannot be opened is stopped before
        // it runs: the child is killed and reaped synchronously, the refusal
        // names verification failure, and no entry is published.
        let copy = copy_test_exe("untracked");
        let started = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let started_hook = std::sync::Arc::clone(&started);
        let track: WindowsTrackFn = std::sync::Arc::new(move |pid| {
            started_hook.lock().unwrap().push(pid);
            Err(std::io::Error::other(
                "the tracking handle cannot be opened",
            ))
        });
        let handler = ControlHandler::default()
            .with_test_operator_app_policy(windows_policy(copy.clone()))
            .with_test_windows_verifier(accept_verifier(
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                "1.2.10",
            ))
            .with_test_windows_track(track);
        let replies = call_app_open(&handler, TEST_APP).await;
        assert_eq!(replies.len(), 1);
        let (code, _) = error_code(&replies[0]);
        assert_eq!(code, subc_control::APP_VERIFICATION_FAILED);
        assert_eq!(
            spawn_attempts(&handler),
            1,
            "the child started before tracking failed"
        );
        assert!(handler.launch_table.live_entry(TEST_APP).is_none());
        assert!(handler.operator_apps.lock().unwrap().is_empty());
        // The started child was stopped, not orphaned: its pid names no
        // live process.
        let started = started.lock().unwrap();
        assert_eq!(started.len(), 1);
        let observed = subc_os::Process::open(started[0])
            .unwrap()
            .and_then(|process| process.observe());
        assert!(observed.is_none(), "a stopped child must not read as live");
        let _ = std::fs::remove_dir_all(copy.parent().unwrap());
    }

    #[test]
    fn windows_spawn_flags_keep_suspend_no_window_and_breakaway() {
        // The production mask is pinned bit by bit: a child that runs before
        // it is bound, flashes a console window, or joins the daemon's root
        // job breaks the launch contract. Tests clear only the breakaway bit,
        // where the job running the tests forbids it; the no-breakaway spawn
        // stays outside a job because a test handler creates none.
        assert_eq!(
            windows_creation_flags(true),
            subc_jobobject::CONTAINMENT_CREATION_FLAGS | 0x0100_0000
        );
        assert_eq!(
            windows_creation_flags(false),
            subc_jobobject::CONTAINMENT_CREATION_FLAGS
        );
    }

    /// A side-effect-free console tool copied to a temp dir, so the test can
    /// attempt a parent swap around it. It exits at once with no arguments,
    /// so a launch that proceeds on the original bytes ends by itself.
    fn swappable_tool() -> PathBuf {
        let tool = PathBuf::from(std::env::var("SystemRoot").expect("SystemRoot is always set"))
            .join("System32")
            .join("whoami.exe");
        assert!(tool.exists(), "whoami.exe must exist: {}", tool.display());
        let dir = std::env::temp_dir().join(format!("subc-app-open-{}-subst", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let copy = dir.join("app.exe");
        std::fs::copy(&tool, &copy).unwrap();
        copy
    }

    #[tokio::test]
    async fn windows_parent_substitution_after_verification_is_refused() {
        // A same-user attacker substitutes the parent directory between
        // verification and process creation. The verifier hook runs exactly
        // in that window, so the swap attempt lands there when the platform
        // allows it. Either way the invariant holds: swapped bytes never
        // run, and a refusal stamps nothing.
        let copy = swappable_tool();
        let swapped = std::sync::Arc::new(std::sync::Mutex::new(false));
        let swapped_hook = std::sync::Arc::clone(&swapped);
        let verify: WindowsVerifyFn = std::sync::Arc::new(
            move |_: &subc_os::ExecutableCapture, request: &WindowsVerifyRequest| {
                let parent = request
                    .path
                    .parent()
                    .expect("the installed file has a parent")
                    .to_path_buf();
                // Best effort: the assertion below branches on whether the swap
                // landed, so a filesystem that pins even the parent directory
                // exercises the no-swap branch instead of failing.
                if std::fs::rename(&parent, parent.with_extension("moved")).is_ok() {
                    *swapped_hook.lock().unwrap() = true;
                }
                Ok("1.2.10".to_owned())
            },
        );
        let handler = ControlHandler::default()
            .with_test_operator_app_policy(windows_policy(copy.clone()))
            .with_test_windows_verifier(verify);
        let replies = call_app_open(&handler, TEST_APP).await;
        let moved = copy.parent().unwrap().with_extension("moved");
        if *swapped.lock().unwrap() {
            // The canonicalized spawn path went stale between verification
            // and process creation, so process creation fails: the launch
            // is refused, the attempted spawn is the only one, and nothing
            // is stamped.
            let (code, _) = error_code(&replies[0]);
            assert_eq!(code, subc_control::APP_VERIFICATION_FAILED);
            assert_eq!(spawn_attempts(&handler), 1);
            assert!(handler.launch_table.live_entry(TEST_APP).is_none());
            assert!(handler.operator_apps.lock().unwrap().is_empty());
        } else {
            // The swap could not be staged here, so the launch proceeds on
            // the original bytes: the tool runs and exits by itself, and no
            // pipe connection means no stamp.
            let body = response_body(&replies[0]);
            assert_eq!(body["status"].as_str(), Some("launched"));
            assert_eq!(spawn_attempts(&handler), 1);
            assert!(handler.operator_apps.lock().unwrap().is_empty());
            wait_for_retire(&handler).await;
        }
        let _ = std::fs::remove_dir_all(&moved);
        let _ = std::fs::remove_dir_all(copy.parent().unwrap());
    }

    #[test]
    fn windows_held_handle_pins_the_file_and_binds_the_spawn() {
        // The held read-sharing handle denies renaming the installed file, so
        // a leaf substitution cannot land while verification and spawn run.
        // A suspended spawn then binds to the pinned bytes without resuming
        // the test binary: the image comparison passes on the original file.
        let copy = copy_test_exe("pin");
        let capture = subc_os::ExecutableCapture::open(&copy).unwrap();
        assert!(
            std::fs::rename(&copy, copy.with_extension("moved")).is_err(),
            "the held handle must pin the installed file"
        );
        let mut command = std::process::Command::new(capture.path());
        subc_jobobject::suspend_on_create(&mut command);
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = command.spawn().unwrap();
        let image = capture.bind(&child).unwrap();
        assert!(
            matches!(
                image.agreement(image.start_time()),
                subc_os::ImageAgreement::Match(_)
            ),
            "the bound image must match the pinned file"
        );
        let _ = child.kill();
        while matches!(child.try_wait(), Ok(None)) {
            std::thread::yield_now();
        }
        let _ = std::fs::remove_dir_all(copy.parent().unwrap());
    }

    #[tokio::test]
    async fn windows_pipe_client_matching_a_live_entry_is_stamped() {
        let handler =
            ControlHandler::default().with_test_operator_app_policy(windows_policy(notepad()));
        let own_pid = std::process::id();
        let incarnation = handler
            .launch_table
            .publish(TEST_APP, own_pid, live_process(), None);
        let (ctx, _rx) = route_ctx(31);
        handler.record_captured_peer(
            ctx.connection_id,
            subc_os::local_peer::CapturedPeer::from(OsPeer::Observed(PeerObservation {
                user: PeerUser::Sid("S-1-5-21-1".to_string()),
                pid: own_pid,
                exe: None,
                signing_id: None,
            })),
        );
        let stamp = handler
            .operator_apps
            .lock()
            .unwrap()
            .get(&ctx.connection_id)
            .cloned()
            .expect("a pipe client of a live entry is stamped");
        assert_eq!(stamp.app.app, TEST_APP);
        assert_eq!(stamp.app.pid, own_pid);
        assert!(handler
            .launch_table
            .is_live_incarnation(TEST_APP, incarnation));

        // A pid that matches no live entry is observed but never stamped.
        let (other, _rx) = route_ctx(33);
        handler.record_captured_peer(
            other.connection_id,
            subc_os::local_peer::CapturedPeer::from(OsPeer::Observed(PeerObservation {
                user: PeerUser::Sid("S-1-5-21-1".to_string()),
                pid: own_pid.wrapping_add(1),
                exe: None,
                signing_id: None,
            })),
        );
        assert!(handler
            .operator_apps
            .lock()
            .unwrap()
            .get(&other.connection_id)
            .is_none());

        // Retiring the entry unstamps future connects, even for the same pid.
        handler.launch_table.retire(TEST_APP);
        let (third, _rx) = route_ctx(35);
        handler.record_captured_peer(
            third.connection_id,
            subc_os::local_peer::CapturedPeer::from(OsPeer::Observed(PeerObservation {
                user: PeerUser::Sid("S-1-5-21-1".to_string()),
                pid: own_pid,
                exe: None,
                signing_id: None,
            })),
        );
        assert!(handler
            .operator_apps
            .lock()
            .unwrap()
            .get(&third.connection_id)
            .is_none());
    }

    #[tokio::test]
    async fn windows_restart_survives_and_reconnects_unstamped() {
        // Installed-service restart at the handler level: entries die with the
        // daemon because they are memory-only. The launched process survives
        // outside the module job, and its new connection is plain `direct`.
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let handler = ControlHandler::default()
            .with_test_operator_app_policy(windows_policy(notepad()))
            .with_test_windows_verifier(accept_verifier(std::sync::Arc::clone(&seen), "1.2.10"));
        let pid = launched_pid_of(&call_app_open(&handler, TEST_APP).await[0]);
        drop(handler);

        let process = subc_os::Process::open(pid)
            .unwrap()
            .expect("the launched process survives the restart");
        assert!(
            process.observe().is_some(),
            "the launched process is still running"
        );
        let restarted =
            ControlHandler::default().with_test_operator_app_policy(windows_policy(notepad()));
        assert!(
            restarted.launch_table.live_entry(TEST_APP).is_none(),
            "a restarted daemon tracks no launch"
        );
        let (ctx, _rx) = route_ctx(37);
        restarted.record_captured_peer(
            ctx.connection_id,
            subc_os::local_peer::CapturedPeer::from(OsPeer::Observed(PeerObservation {
                user: PeerUser::Sid("S-1-5-21-1".to_string()),
                pid,
                exe: None,
                signing_id: None,
            })),
        );
        assert!(
            restarted
                .operator_apps
                .lock()
                .unwrap()
                .get(&ctx.connection_id)
                .is_none(),
            "a post-restart connection is unstamped"
        );
        stop_windows_process(pid);
    }
}
