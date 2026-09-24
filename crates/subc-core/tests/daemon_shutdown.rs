#![cfg(unix)]

use std::{
    fs,
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use subc_daemon::{read_frame, test_support::TestTempDir, write_frame, Frame};
use subc_protocol::{Flags, FrameType, Priority};

mod common;

// Real daemons compete with other integration binaries for spawn/registration
// resources. Serialize this file instead of widening their startup deadlines.
static DAEMON_GATE: Mutex<()> = Mutex::new(());

struct Fixture {
    root: TestTempDir,
    child: Child,
    _permit: MutexGuard<'static, ()>,
}

impl Fixture {
    fn boot(busy: bool) -> Self {
        Self::boot_with(busy, json!({}), None)
    }

    /// `observer` is merged into the `shutdown-observer` module's config, and
    /// its `env` into that module's environment. `none_module`, when given,
    /// adds a `protocol: "none"` module named `wire-less` whose stub never
    /// connects, merged the same way.
    fn boot_with(busy: bool, observer: Value, none_module: Option<Value>) -> Self {
        let permit = DAEMON_GATE.lock().unwrap_or_else(|p| p.into_inner());
        let root = fresh_dir("daemon-shutdown");
        for dir in ["config/cortexkit", "runtime", "data/cortexkit/run"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let mut modules = json!({ "shutdown-observer": {
            "program": env!("CARGO_BIN_EXE_fake-aft-stub"),
            "env": {
                "FAKE_AFT_MODULE_ID": "shutdown-observer",
                "FAKE_AFT_EVENTS_PATH": root.join("events.jsonl"),
                "FAKE_AFT_PID_PATH": root.join("observer.pid"),
                "FAKE_AFT_RECORD_EOF": "1",
                "FAKE_AFT_EOF_TEARDOWN_MS": OBSERVER_TEARDOWN_MS.to_string(),
                "FAKE_AFT_DELAY_FROM_BODY": "1",
                "FAKE_AFT_SHUTDOWN_JOURNAL": root.join("data/cortexkit/run/terminals.jsonl"),
                "FAKE_AFT_BUSY_GAUGES": "work",
                "FAKE_AFT_HEALTH_METRICS": if busy { "{\"work\":1}" } else { "{\"work\":0}" }
            }
        }});
        merge_module(&mut modules["shutdown-observer"], &observer);
        if let Some(extra) = &none_module {
            modules["wire-less"] = json!({
                "program": env!("CARGO_BIN_EXE_fake-aft-stub"),
                "protocol": "none",
                "env": {
                    "FAKE_AFT_NEVER_CONNECT": "1",
                    "FAKE_AFT_PID_PATH": root.join("wire-less.pid"),
                    "FAKE_AFT_NEVER_CONNECT_READY_PATH": root.join("wire-less.ready"),
                },
            });
            merge_module(&mut modules["wire-less"], extra);
        }
        fs::write(
            root.join("config/cortexkit/subc.jsonc"),
            serde_json::to_vec(&json!({ "version": 1, "modules": modules })).unwrap(),
        )
        .unwrap();
        let child = spawn_daemon(&root);
        let mut fixture = Self {
            root,
            child,
            _permit: permit,
        };
        fixture.wait_ready(none_module.is_some());
        fixture
    }

    /// Start another daemon over the same directories, after the previous one
    /// has exited, and wait for it the way `boot_with` does.
    fn restart_daemon(&mut self, none_module: bool) {
        assert!(
            self.child.try_wait().unwrap().is_some(),
            "the previous daemon must have exited first"
        );
        // Fresh markers, so the waits below see the new daemon's children.
        for file in ["observer.pid", "wire-less.pid", "wire-less.ready"] {
            let _ = fs::remove_file(self.root.join(file));
        }
        self.child = spawn_daemon(&self.root);
        self.wait_ready(none_module);
    }

    fn wait_ready(&mut self, none_module: bool) {
        let fixture = self;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                fixture.child.try_wait().unwrap().is_none(),
                "daemon exited during startup"
            );
            if fixture.connection().exists() {
                let output = Command::new(env!("CARGO_BIN_EXE_ck"))
                    .arg("--subc")
                    .arg(fixture.connection())
                    .args(["module", "status", "shutdown-observer", "--json"])
                    .output()
                    .unwrap();
                if serde_json::from_slice::<Value>(&output.stdout)
                    .is_ok_and(|status| status["module"]["live"] == true)
                {
                    break;
                }
            }
            assert!(Instant::now() < deadline, "module did not register");
            thread::sleep(Duration::from_millis(10));
        }
        if none_module {
            // The ready file is written only after the stub's SIGTERM handler
            // is installed; signalling earlier would meet the default
            // disposition and say nothing about the daemon.
            while !fixture.root.join("wire-less.ready").exists() {
                assert!(
                    Instant::now() < deadline,
                    "protocol none module never parked"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn pid_of(&self, file: &str) -> i32 {
        fs::read_to_string(self.root.join(file))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    /// SIGKILL the daemon's process group, as launchd does to a job's group
    /// once the job's main process has exited.
    fn kill_daemon_group(&self) {
        let _ = rustix::process::kill_process_group(
            rustix::process::Pid::from_raw(self.child.id() as i32).unwrap(),
            rustix::process::Signal::KILL,
        );
    }

    fn connection(&self) -> PathBuf {
        self.root
            .join("runtime")
            .join(subc_transport::CONNECTION_FILE_NAME)
    }

    fn term(&self) {
        rustix::process::kill_process(
            rustix::process::Pid::from_raw(self.child.id() as i32).unwrap(),
            rustix::process::Signal::TERM,
        )
        .unwrap();
    }

    fn events(&self) -> Vec<Value> {
        fs::read_to_string(self.root.join("events.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn wait_event(&self, kind: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            if let Some(event) = self
                .events()
                .into_iter()
                .find(|event| event["kind"] == kind)
            {
                return event;
            }
            assert!(
                Instant::now() < deadline,
                "module never observed {kind}: {:?}",
                self.events()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_exit(&mut self, budget: Duration) {
        let deadline = Instant::now() + budget;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "SIGTERM must run the handler, not kill by default: {status}"
                );
                return;
            }
            assert!(
                Instant::now() < deadline,
                "daemon exceeded its shutdown budget"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // A child that outlived the daemon (the failure these tests exist to
        // catch) must not outlive the test as well.
        for file in ["observer.pid", "wire-less.pid"] {
            if let Some(pid) = fs::read_to_string(self.root.join(file))
                .ok()
                .and_then(|pid| pid.trim().parse::<i32>().ok())
                .and_then(rustix::process::Pid::from_raw)
            {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
        }
    }
}

/// Start the shipped daemon binary with every per-user location inside `root`.
///
/// The daemon leads its own process group, as it does under launchd (which
/// starts each job in a new session). That is what lets a test compare a
/// module's group against the daemon's, and reproduce the service manager's
/// kill of that group after the daemon exits.
fn spawn_daemon(root: &TestTempDir) -> Child {
    Command::new(env!("CARGO_BIN_EXE_ck-subc"))
        .process_group(0)
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("SUBC_PORT", "0")
        .env("SUBC_CGROUP_PLACEMENT", "disabled")
        .env_remove("CK_LOG")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// Merges `extra` into a module's config: `env` entries into its environment,
/// every other key onto the module itself.
fn merge_module(module: &mut Value, extra: &Value) {
    for (key, value) in extra.as_object().unwrap() {
        if key == "env" {
            for (name, entry) in value.as_object().unwrap() {
                module["env"][name] = entry.clone();
            }
        } else {
            module[key] = value.clone();
        }
    }
}

/// A scratch directory no other test, and no earlier run, can share.
///
/// `TestTempDir` names a directory `<label>-<pid>-<counter>`, which is unique
/// among live test processes, but it keeps a failed test's directory on disk
/// and `create_dir_all` accepts an existing one. A later run whose pid and
/// counter repeat would then read a stale `events.jsonl`, and a leftover
/// `teardown_complete` or pid file would pass or fail a test for reasons that
/// have nothing to do with the daemon. The nanosecond stamp makes the name new,
/// and the emptiness check refuses to run if it somehow is not.
fn fresh_dir(label: &str) -> TestTempDir {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = TestTempDir::new(&format!("{label}-{nanos}"));
    assert!(
        fs::read_dir(&*dir).unwrap().next().is_none(),
        "scratch directory {} already has contents",
        dir.display()
    );
    dir
}

fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Simulated EOF teardown in the observer stub. Long enough that a group kill
/// arriving with the EOF lands mid-teardown; short enough to fit the daemon's
/// child-exit grace.
const OBSERVER_TEARDOWN_MS: u64 = 200;

fn process_alive(pid: i32) -> bool {
    // Signal 0 checks existence without delivering anything.
    rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap()).is_ok()
}

#[test]
fn module_leads_its_own_process_group_not_the_daemons() {
    let fixture = Fixture::boot(false);
    let module = fixture.pid_of("observer.pid");
    let pgid = rustix::process::getpgid(rustix::process::Pid::from_raw(module))
        .unwrap()
        .as_raw_nonzero()
        .get();
    assert_ne!(
        pgid,
        fixture.child.id() as i32,
        "a module in the daemon's process group dies with the service manager's group kill"
    );
    assert_eq!(pgid, module, "a module must lead its own process group");
}

#[test]
fn module_finishes_its_eof_teardown_before_the_service_manager_group_kill() {
    let mut fixture = Fixture::boot(false);
    fixture.term();
    fixture.wait_exit(Duration::from_secs(5));
    // What launchd does the moment the job's main process exits.
    fixture.kill_daemon_group();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let kinds: Vec<String> = fixture
            .events()
            .into_iter()
            .filter_map(|e| e["kind"].as_str().map(str::to_owned))
            .filter(|kind| kind == "eof" || kind == "teardown_complete")
            .collect();
        if kinds == ["eof", "teardown_complete"] {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "module did not complete its EOF teardown across a daemon stop: {kinds:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// BROCA's EOF teardown seals in-flight runs and took 12 s at a measured cut.
/// A module whose teardown outlasts a short fixed bound must be left to finish
/// it, not signalled while it works.
#[test]
fn slow_eof_teardown_within_its_drain_budget_finishes_unsignalled() {
    let mut fixture = Fixture::boot_with(
        false,
        json!({
            "drain_timeout_ms": 6000,
            "env": { "FAKE_AFT_EOF_TEARDOWN_MS": "3000", "FAKE_AFT_RECORD_SIGTERM": "1" }
        }),
        None,
    );
    let observer = fixture.pid_of("observer.pid");
    let started = Instant::now();
    fixture.term();
    fixture.wait_exit(Duration::from_secs(10));
    let elapsed = started.elapsed();
    let kinds: Vec<String> = fixture
        .events()
        .into_iter()
        .filter_map(|e| e["kind"].as_str().map(str::to_owned))
        .filter(|kind| kind == "eof" || kind == "teardown_complete" || kind == "sigterm")
        .collect();
    assert_eq!(
        kinds,
        ["eof", "teardown_complete"],
        "a module inside its drain budget must finish its EOF teardown with no signal"
    );
    assert!(
        elapsed >= Duration::from_millis(3000),
        "the daemon did not wait for the module's teardown: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(6000),
        "the daemon waited out the deadline instead of returning when the module exited: {elapsed:?}"
    );
    assert!(
        !process_alive(observer),
        "the module must have exited on its own"
    );
}

#[test]
fn eof_ignoring_module_is_sigtermed_at_its_deadline_then_killed() {
    let mut fixture = Fixture::boot_with(
        false,
        json!({
            "drain_timeout_ms": 1000,
            "env": { "FAKE_AFT_IGNORE_EOF": "1", "FAKE_AFT_RECORD_SIGTERM": "1" }
        }),
        None,
    );
    let observer = fixture.pid_of("observer.pid");
    let started = Instant::now();
    let term_ms = unix_ms_now();
    fixture.term();
    fixture.wait_exit(Duration::from_secs(6));
    let exit_ms = unix_ms_now();
    let elapsed = started.elapsed();
    let sigterms: Vec<u64> = fixture
        .events()
        .into_iter()
        .filter(|e| e["kind"] == "sigterm")
        .map(|e| e["at_ms"].as_u64().unwrap())
        .collect();
    assert_eq!(
        sigterms.len(),
        1,
        "exactly one SIGTERM, at the deadline: {sigterms:?}"
    );
    // The deadline is counted from the start of the child stop, which follows
    // the notice and drain, so it lands at least 1 s after the daemon's SIGTERM.
    assert!(
        sigterms[0] >= term_ms + 1000,
        "SIGTERM came before the module's 1 s deadline: {} ms after",
        sigterms[0] - term_ms
    );
    let term_to_exit = exit_ms - sigterms[0];
    assert!(
        (400..1500).contains(&term_to_exit),
        "SIGKILL must follow the deadline SIGTERM by 0.5 s: {term_to_exit} ms"
    );
    // 1 s deadline + 0.5 s + 0.25 s reap, after a notice and drain of at most
    // 2.5 s that a quiescent module does not spend.
    assert!(
        elapsed < Duration::from_millis(4500),
        "shutdown overran the module's bound: {elapsed:?}"
    );
    assert!(
        !process_alive(observer),
        "the module must not outlive the daemon"
    );
}

#[test]
fn protocol_none_child_is_stopped_by_sigterm_not_left_running() {
    let marker = fresh_dir("wire-less-marker");
    let marker_path = marker.join("sigterm");
    let mut fixture = Fixture::boot_with(
        false,
        json!({}),
        Some(json!({ "env": { "FAKE_AFT_SIGTERM_MARKER_PATH": marker_path } })),
    );
    let wire_less = fixture.pid_of("wire-less.pid");
    fixture.term();
    fixture.wait_exit(Duration::from_secs(5));
    assert!(
        !process_alive(wire_less),
        "a protocol none child must not outlive the daemon"
    );
    assert_eq!(
        fs::read_to_string(&marker_path).ok().as_deref(),
        Some("sigterm\n"),
        "the protocol none child must be asked to stop with SIGTERM"
    );
}

#[test]
fn child_ignoring_sigterm_is_killed_within_the_shutdown_bound() {
    let mut fixture = Fixture::boot_with(
        false,
        json!({}),
        Some(json!({ "drain_timeout_ms": 1500, "env": { "FAKE_AFT_IGNORE_SIGTERM": "1" } })),
    );
    let wire_less = fixture.pid_of("wire-less.pid");
    let started = Instant::now();
    fixture.term();
    // Notice and drain return early for a quiescent module; the child stop
    // adds the child's 1.5 s drain budget, then SIGKILL and at most 0.25 s to
    // reap.
    fixture.wait_exit(Duration::from_secs(5));
    assert!(
        started.elapsed() >= Duration::from_millis(1400),
        "the child was given no grace before the kill"
    );
    assert!(
        !process_alive(wire_less),
        "a child ignoring SIGTERM must still not outlive the daemon"
    );
}

/// A daemon killed outright runs no shutdown stop, and modules lead their own
/// process groups, so its `protocol: "none"` child outlives it. The next daemon
/// must end that orphan, from the live-children record the first one kept,
/// before it spawns the module again: two copies would fight over one port.
#[test]
fn orphan_of_a_killed_daemon_is_ended_by_the_next_daemon_before_it_respawns() {
    let marker = fresh_dir("orphan-marker");
    let marker_path = marker.join("sigterm");
    let mut fixture = Fixture::boot_with(
        false,
        json!({}),
        Some(json!({ "env": { "FAKE_AFT_SIGTERM_MARKER_PATH": marker_path } })),
    );
    let orphan = fixture.pid_of("wire-less.pid");
    fixture.kill_daemon_group();
    fixture.child.wait().unwrap();
    assert!(
        process_alive(orphan),
        "precondition: a protocol none child outlives a SIGKILLed daemon"
    );
    assert!(
        !marker_path.exists(),
        "nothing has signalled the orphan yet"
    );

    fixture.restart_daemon(true);
    let replacement = fixture.pid_of("wire-less.pid");
    assert_ne!(replacement, orphan, "the module was spawned again");
    assert!(
        !process_alive(orphan),
        "the next daemon must end the previous daemon's orphan"
    );
    assert_eq!(
        fs::read_to_string(&marker_path).ok().as_deref(),
        Some("sigterm\n"),
        "the orphan must be asked to stop with SIGTERM first"
    );
    // Both daemons log into the same file; look only at the second one's lines.
    let log = daemon_log(&fixture);
    let second_boot = log
        .rfind("subc daemon starting")
        .expect("the second daemon logged its start");
    let log = &log[second_boot..];
    let swept = log
        .find("orphan sweep: previous daemon's child exited after SIGTERM")
        .unwrap_or_else(|| panic!("no sweep decision in the second daemon's log:\n{log}"));
    let supervised = log
        .find("configured module supervised")
        .expect("the second daemon supervised its modules");
    assert!(
        swept < supervised,
        "the orphan must be gone before any module is spawned again:\n{log}"
    );
}

#[test]
fn provider_observes_draining_before_its_own_eof() {
    let mut fixture = Fixture::boot(false);
    fixture.term();
    fixture.wait_exit(Duration::from_secs(4));
    fixture.wait_event("eof");
    let sequence: Vec<_> = fixture
        .events()
        .into_iter()
        .filter_map(|e| e["kind"].as_str().map(str::to_owned))
        .filter(|kind| kind == "draining" || kind == "eof")
        .collect();
    assert_eq!(
        sequence,
        ["draining", "eof"],
        "provider must observe notice before losing its wire"
    );
}

#[test]
fn shutdown_marker_is_durable_and_precedes_provider_notice() {
    let mut fixture = Fixture::boot(false);
    let incarnation = subc_transport::read_for_client(fixture.connection())
        .unwrap()
        .daemon_id;
    fixture.term();
    fixture.wait_exit(Duration::from_secs(4));
    assert_eq!(fixture.wait_event("draining")["shutdown_marker_seen"], true);
    let journal =
        fs::read_to_string(fixture.root.join("data/cortexkit/run/terminals.jsonl")).unwrap();
    let markers: Vec<Value> = journal
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|entry| entry["event"] == "daemon_shutdown")
        .collect();
    assert_eq!(markers.len(), 1);
    assert_eq!(
        markers[0]["daemon_incarnation"],
        format!("{:032x}", u128::from_be_bytes(incarnation))
    );
    assert!(markers[0]["at_ms"].as_u64().unwrap() > 0);
}

#[test]
fn stubborn_provider_gets_a_bounded_drain_not_a_completion_promise() {
    let mut fixture = Fixture::boot(true);
    let started = Instant::now();
    fixture.term();
    fixture.wait_exit(Duration::from_secs(4));
    assert!(
        started.elapsed() >= Duration::from_millis(1800),
        "busy provider did not get its drain wait"
    );
}

#[test]
fn quiescent_provider_does_not_spend_the_whole_drain_budget() {
    let mut fixture = Fixture::boot(false);
    fixture.term();
    fixture.wait_exit(Duration::from_millis(1500));
}

/// Every line of the durable terminal journal, in file order.
fn journal(fixture: &Fixture) -> Vec<Value> {
    fs::read_to_string(fixture.root.join("data/cortexkit/run/terminals.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The observer's terminal records written after the daemon's shutdown
/// marker. Panics if there is no marker, which would make "nothing after it"
/// vacuous.
fn observer_records_after_marker(fixture: &Fixture) -> Vec<Value> {
    let lines = journal(fixture);
    let marker = lines
        .iter()
        .position(|line| line["event"] == "daemon_shutdown")
        .unwrap_or_else(|| panic!("no daemon_shutdown marker in the journal: {lines:?}"));
    lines[marker + 1..]
        .iter()
        .filter(|line| line["module_id"] == "shutdown-observer")
        .cloned()
        .collect()
}

/// The daemon's own log, every dated file concatenated.
fn daemon_log(fixture: &Fixture) -> String {
    let mut files: Vec<PathBuf> = fs::read_dir(fixture.root.join("data/cortexkit/run/logs"))
        .map(|dir| {
            dir.filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("subc"))
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
        .iter()
        .map(|path| fs::read_to_string(path).unwrap_or_default())
        .collect()
}

/// The supervisor's reap path, not only the record, must know the exit came
/// from the shutdown: its crash arm is what schedules a respawn.
fn assert_reap_saw_daemon_shutdown(fixture: &Fixture) {
    let log = daemon_log(fixture);
    assert!(
        log.contains("supervised module exited during daemon shutdown"),
        "the reap path did not treat the exit as part of the daemon shutdown:\n{log}"
    );
    assert!(
        !log.contains("exited abnormally (crash)"),
        "an exit during daemon shutdown was handled as a crash:\n{log}"
    );
}

fn signal_pid(pid: i32, signal: rustix::process::Signal) {
    rustix::process::kill_process(rustix::process::Pid::from_raw(pid).unwrap(), signal).unwrap();
}

/// A module that exits nonzero on the EOF of an ordered shutdown is exiting
/// because the daemon is going away. It must be recorded as a daemon shutdown,
/// not a crash, and never respawned.
#[test]
fn module_exiting_nonzero_on_shutdown_eof_is_recorded_as_daemon_shutdown_and_not_respawned() {
    let mut fixture = Fixture::boot_with(
        false,
        json!({ "env": { "FAKE_AFT_EOF_EXIT_CODE": "1" } }),
        None,
    );
    let observer = fixture.pid_of("observer.pid");
    fixture.term();
    fixture.wait_exit(Duration::from_secs(5));
    fixture.wait_event("teardown_complete");

    let records = observer_records_after_marker(&fixture);
    assert_eq!(
        records.len(),
        1,
        "exactly one exit after the marker: {records:?}"
    );
    assert_eq!(
        records[0]["exit_code"], 1,
        "the module must have exited nonzero, or this test does not reach the crash path: {records:?}"
    );
    assert_eq!(
        records[0]["disposition"], "daemon_shutdown",
        "an exit during daemon shutdown is not a crash to restart from: {records:?}"
    );
    assert!(
        records.iter().all(|r| r["disposition"] != "restarting"),
        "no exit after the shutdown marker may be recorded as restarting: {records:?}"
    );
    assert_eq!(
        fixture.pid_of("observer.pid"),
        observer,
        "the module was respawned during daemon shutdown"
    );
    assert_reap_saw_daemon_shutdown(&fixture);
}

/// A systemd unit with KillMode=control-group signals every module at the same
/// moment as the daemon. A module killed that way, while the daemon is still
/// in its notice and drain, must be recorded as a daemon shutdown and must not
/// be respawned into a daemon that is going away.
#[test]
fn module_signalled_during_daemon_shutdown_is_recorded_as_daemon_shutdown_and_not_respawned() {
    // Busy, so the drain holds the daemon for its full budget: long enough
    // for a respawn (100 ms of backoff) to happen if the supervisor allowed it.
    let mut fixture = Fixture::boot(true);
    let observer = fixture.pid_of("observer.pid");
    fixture.term();
    // The notice has been sent, so the daemon has begun its shutdown and not
    // yet closed any connection.
    fixture.wait_event("draining");
    signal_pid(observer, rustix::process::Signal::TERM);
    fixture.wait_exit(Duration::from_secs(8));

    let records = observer_records_after_marker(&fixture);
    assert!(
        !records.is_empty(),
        "the signalled module's exit was never recorded"
    );
    assert_eq!(
        records[0]["exit_signal"], 15,
        "the first exit after the marker must be the SIGTERM this test sent: {records:?}"
    );
    assert!(
        records
            .iter()
            .all(|r| r["disposition"] == "daemon_shutdown"),
        "every exit after the shutdown marker is a daemon shutdown: {records:?}"
    );
    assert_eq!(
        fixture.pid_of("observer.pid"),
        observer,
        "the module was respawned during daemon shutdown"
    );
    assert_reap_saw_daemon_shutdown(&fixture);
}

/// The control for the two above: the same signal while the daemon is NOT
/// shutting down is a crash, recorded as `restarting`, and respawned.
#[test]
fn module_signalled_while_the_daemon_runs_is_a_crash_and_is_respawned() {
    let fixture = Fixture::boot(false);
    let observer = fixture.pid_of("observer.pid");
    signal_pid(observer, rustix::process::Signal::TERM);
    let deadline = Instant::now() + Duration::from_secs(5);
    while fixture.pid_of("observer.pid") == observer {
        assert!(
            Instant::now() < deadline,
            "a crashed module was not respawned"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let lines = journal(&fixture);
    assert!(
        lines.iter().all(|line| line["event"] != "daemon_shutdown"),
        "no shutdown marker while the daemon runs: {lines:?}"
    );
    let records: Vec<&Value> = lines
        .iter()
        .filter(|line| line["module_id"] == "shutdown-observer")
        .collect();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["exit_signal"], 15, "{records:?}");
    assert_eq!(records[0]["exit_kind"], "crash", "{records:?}");
    assert_eq!(records[0]["disposition"], "restarting", "{records:?}");
}

#[test]
fn second_sigterm_cuts_short_a_stubborn_provider_wait() {
    let mut fixture = Fixture::boot(true);
    fixture.term();
    fixture.wait_event("draining");
    assert!(fixture.child.try_wait().unwrap().is_none());
    fixture.term();
    fixture.wait_exit(Duration::from_millis(800));
}

async fn open_route(fixture: &Fixture) -> (tokio::net::TcpStream, u16, u32) {
    let mut stream = common::connect_authed_client(fixture.connection())
        .await
        .unwrap();
    let request = json!({"op":"route.open", "target":{"kind":"tool_provider", "module_id":"shutdown-observer"},
        "identity":{"project_root":fixture.root.to_path_buf(), "harness":"shutdown-test", "session":"shutdown-test"}});
    write_frame(
        &mut stream,
        &Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Interactive, false),
            0,
            0,
            1,
            serde_json::to_vec(&request).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let response = tokio::time::timeout(Duration::from_secs(4), read_frame(&mut stream))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        response.header.ty,
        FrameType::Response,
        "route open failed: {:?}",
        response.body
    );
    let body: Value = serde_json::from_slice(&response.body).unwrap();
    (
        stream,
        body["route_channel"].as_u64().unwrap() as u16,
        body["route_epoch"].as_u64().unwrap() as u32,
    )
}

async fn route_closed(stream: &mut tokio::net::TcpStream) -> Value {
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let frame = read_frame(stream)
                .await
                .unwrap()
                .expect("client disconnected before route.closed");
            if let Ok(body) = serde_json::from_slice::<Value>(&frame.body) {
                if body["op"] == "route.closed" {
                    assert_eq!(frame.header.channel, 0);
                    return body;
                }
            }
        }
    })
    .await
    .expect("timed out waiting for route.closed")
}

#[tokio::test]
async fn route_closed_during_daemon_shutdown_reports_restart() {
    let mut fixture = Fixture::boot(true);
    let (mut stream, _, _) = open_route(&fixture).await;
    let observer = fixture.pid_of("observer.pid");
    fixture.term();
    fixture.wait_event("draining");
    signal_pid(observer, rustix::process::Signal::TERM);
    let closed = route_closed(&mut stream).await;
    assert_eq!(closed["module_id"], "shutdown-observer");
    assert_eq!(closed["reason"], "restart");
    assert_eq!(closed["terminal"], false);
    fixture.wait_exit(Duration::from_secs(8));
}

#[tokio::test]
async fn route_closed_after_unexpected_module_loss_reports_crash() {
    let fixture = Fixture::boot(false);
    let (mut stream, _, _) = open_route(&fixture).await;
    signal_pid(
        fixture.pid_of("observer.pid"),
        rustix::process::Signal::TERM,
    );
    let closed = route_closed(&mut stream).await;
    assert_eq!(closed["module_id"], "shutdown-observer");
    assert_eq!(closed["reason"], "crash");
}

#[tokio::test]
async fn consumer_observes_route_closing_on_channel_zero_before_eof() {
    let mut fixture = Fixture::boot(false);
    let (mut stream, _, _) = open_route(&fixture).await;
    fixture.term();
    let mut sequence = Vec::new();
    tokio::time::timeout(Duration::from_secs(4), async {
        while let Some(frame) = read_frame(&mut stream).await.unwrap() {
            if serde_json::from_slice::<Value>(&frame.body)
                .is_ok_and(|body| body["op"] == "route.closing")
            {
                assert_eq!(frame.header.channel, 0);
                sequence.push("route.closing");
            }
        }
        sequence.push("eof");
    })
    .await
    .unwrap();
    assert_eq!(sequence, ["route.closing", "eof"]);
    fixture.wait_exit(Duration::from_secs(1));
}

#[tokio::test]
async fn in_flight_reply_can_finish_during_the_shutdown_drain() {
    let mut fixture = Fixture::boot(false);
    let (mut stream, channel, epoch) = open_route(&fixture).await;
    write_frame(
        &mut stream,
        &Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Interactive, false),
            channel,
            epoch,
            2,
            br#"{"delay_ms":750}"#.to_vec(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    fixture.wait_event("request_received");
    fixture.term();
    let mut sequence = Vec::new();
    tokio::time::timeout(Duration::from_secs(4), async {
        while let Some(frame) = read_frame(&mut stream).await.unwrap() {
            if frame.header.ty == FrameType::Response && frame.header.corr == 2 {
                sequence.push("reply");
            }
        }
        sequence.push("eof");
    })
    .await
    .unwrap();
    assert_eq!(sequence, ["reply", "eof"]);
    fixture.wait_exit(Duration::from_secs(1));
}

#[cfg(target_os = "linux")]
#[test]
fn binary_disabled_placement_leaves_callers_cgroup_untouched() {
    let root = fresh_dir("binary-cgroup-disabled");
    for dir in ["config/cortexkit", "runtime", "data"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    let module_id = format!("cgroup-binary-probe-{}", std::process::id());
    let pid_file = root.join("module.pid");
    fs::write(
        root.join("config/cortexkit/subc.jsonc"),
        serde_json::to_vec(&json!({"version": 1, "modules": {
            (module_id.clone()): {"program": env!("CARGO_BIN_EXE_fake-aft-stub"),
                "env": {"FAKE_AFT_MODULE_ID": &module_id,
                        "FAKE_AFT_PID_PATH": &pid_file}}
        }}))
        .unwrap(),
    )
    .unwrap();
    let relative = fs::read_to_string("/proc/self/cgroup")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .expect("cgroup v2 hierarchy")
        .trim_start_matches('/')
        .to_string();
    let modules = PathBuf::from("/sys/fs/cgroup")
        .join(relative)
        .join("subc-modules");
    let parent_existed = modules.exists();
    assert!(
        !modules.join(&module_id).exists(),
        "probe cgroup predates test"
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_ck-subc"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("SUBC_PORT", "0")
        .env("SUBC_CGROUP_PLACEMENT", "disabled")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pid_file.exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "daemon exited before module launch"
        );
        assert!(Instant::now() < deadline, "fixture module never launched");
        thread::sleep(Duration::from_millis(10));
    }
    let created_parent = !parent_existed && modules.exists();
    let created_module = modules.join(&module_id).exists();
    child.kill().unwrap();
    child.wait().unwrap();
    if let Ok(pid) = fs::read_to_string(&pid_file) {
        if let Ok(pid) = pid.trim().parse::<i32>() {
            if let Some(pid) = rustix::process::Pid::from_raw(pid) {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
        }
    }
    assert!(
        !created_parent,
        "daemon created subc-modules in caller's cgroup"
    );
    assert!(
        !created_module,
        "daemon placed fixture under caller's cgroup"
    );
}

#[test]
fn binary_rejects_invalid_cgroup_override_before_writing_state() {
    let root = fresh_dir("binary-cgroup-invalid");
    let output = Command::new(env!("CARGO_BIN_EXE_ck-subc"))
        .env("SUBC_CGROUP_PLACEMENT", "current")
        .env("XDG_DATA_HOME", root.join("data"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("SUBC_CGROUP_PLACEMENT"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !root.join("data").exists(),
        "startup wrote state before refusing"
    );
}
