#![cfg(unix)]

use cortexkit_test_support::{dev_command, process_alive, wait_until_gone, ScratchDir};
use std::{
    fs,
    os::unix::{fs::MetadataExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Stdio},
    sync::{Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use subc_daemon::{read_frame, write_frame, Frame};
use subc_protocol::{Flags, FrameType, Priority};

mod common;

// Real daemons compete with other integration binaries for spawn/registration
// resources. Serialize this file instead of widening their startup deadlines.
static DAEMON_GATE: Mutex<()> = Mutex::new(());

// These are fixture liveness limits, not the daemon's drain policy. Socket EOF
// precedes the child's teardown and reap, which can be descheduled under load.
const SHUTDOWN_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(15);

struct Fixture {
    root: ScratchDir,
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
                let mut command = dev_command(common::ckdev_ck());
                common::kit::isolate_client(&mut command, &fixture.root.join("client"));
                let output = command
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
                "daemon did not exit within fixture limit {budget:?}\nevents: {:?}\ndaemon log:\n{}",
                self.events(),
                daemon_log(self)
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
        for file in [
            "observer.pid",
            "wire-less.pid",
            "provider.pid",
            "consumer.pid",
        ] {
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
fn spawn_daemon(root: &ScratchDir) -> Child {
    dev_command(common::ckdev_subc())
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
/// `cortexkit_test_support::ScratchDir` names a directory `<label>-<pid>-<counter>`, which is unique
/// among live test processes, but it keeps a failed test's directory on disk
/// and `create_dir_all` accepts an existing one. A later run whose pid and
/// counter repeat would then read a stale `events.jsonl`, and a leftover
/// `teardown_complete` or pid file would pass or fail a test for reasons that
/// have nothing to do with the daemon. The nanosecond stamp makes the name new,
/// and the emptiness check refuses to run if it somehow is not.
fn fresh_dir(label: &str) -> ScratchDir {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = ScratchDir::new(&format!("{label}-{nanos}"));
    assert!(
        fs::read_dir(&*dir).unwrap().next().is_none(),
        "scratch directory {} already has contents",
        dir.display()
    );
    dir
}

fn child_is_recorded(path: &Path, pid: u32, observed: &subc_os::Observation) -> bool {
    let Some(executable) = observed.executable else {
        return false;
    };
    let Some(record) = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return false;
    };
    record["version"] == 1
        && record["children"].as_array().is_some_and(|children| {
            children.iter().any(|child| {
                child["module_id"] == "wire-less"
                    && child["protocol"] == "none"
                    && child["pid"] == pid
                    && child["start_time"] == observed.start_time
                    && child["executable"]["device"] == executable.device
                    && child["executable"]["inode"] == executable.inode
            })
        })
}

fn recorded_child_diagnostics(
    path: &Path,
    process: &subc_os::Process,
    observed: &subc_os::Observation,
    program: &Path,
) -> String {
    let pid = process.pid();
    let program_identity = fs::metadata(program).map(|stat| (stat.dev(), stat.ino()));
    #[cfg(target_os = "linux")]
    let proc_exe = fs::read_link(format!("/proc/{pid}/exe"));
    #[cfg(not(target_os = "linux"))]
    let proc_exe = "unavailable on this platform";
    format!(
        "the daemon never persisted the child's matching identity: \
         pid={pid}, initial observation={observed:?}, current observation={:?}, \
         record={:?}, configured program={program:?}, \
         program stat (device, inode)={program_identity:?}, /proc/{pid}/exe={proc_exe:?}",
        process.observe(),
        fs::read_to_string(path),
    )
}

fn wait_for_recorded_child(path: &Path, pid: u32, program: &Path) {
    let process = subc_os::Process::open(pid)
        .unwrap()
        .expect("the child must still exist before its daemon is killed");
    let observed = process.observe().expect("the child must still be running");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !child_is_recorded(path, pid, &observed) {
        if Instant::now() >= deadline {
            panic!(
                "{}",
                recorded_child_diagnostics(path, &process, &observed, program)
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn recorded_child_timeout_reports_both_identities_and_executable_paths() {
    let root = fresh_dir("recorded-child-diagnostics");
    let record = root.join("live-children.json");
    let child = json!({
        "module_id": "wire-less", "protocol": "none", "pid": std::process::id(),
        "start_time": 41, "executable": { "device": 43, "inode": 47 }
    });
    let record_text = json!({ "version": 1, "children": [child] }).to_string();
    fs::write(&record, &record_text).unwrap();
    let process = subc_os::Process::open(std::process::id()).unwrap().unwrap();
    let observed = subc_os::Observation {
        start_time: 23,
        executable: Some(subc_os::FileIdentity {
            device: 29,
            inode: 31,
        }),
    };
    let program = std::env::current_exe().unwrap();
    let stat = fs::metadata(&program).unwrap();
    let diagnostics = recorded_child_diagnostics(&record, &process, &observed, &program);
    for expected in [
        format!("pid={}", process.pid()),
        "initial observation=Observation { start_time: 23, executable: Some(FileIdentity { device: 29, inode: 31 }) }".into(),
        "current observation=Some(Observation".into(),
        format!("record=Ok({record_text:?})"),
        format!("configured program={program:?}"),
        format!("program stat (device, inode)=Ok(({}, {}))", stat.dev(), stat.ino()),
    ] {
        assert!(diagnostics.contains(&expected), "missing {expected:?}: {diagnostics}");
    }
    #[cfg(target_os = "linux")]
    {
        let exe = fs::read_link(format!("/proc/{}/exe", process.pid())).unwrap();
        assert!(diagnostics.contains(&format!("/proc/{}/exe=Ok({exe:?})", process.pid())));
    }
}

#[test]
fn ready_child_requires_its_matching_identity_in_the_persisted_record() {
    let root = fresh_dir("recorded-child");
    let record = root.join("live-children.json");
    let pending = root.join("pending.json");
    let observed = subc_os::Observation {
        start_time: 23,
        executable: Some(subc_os::FileIdentity {
            device: 29,
            inode: 31,
        }),
    };
    let child = json!({
        "module_id": "wire-less", "protocol": "none", "pid": 17,
        "start_time": 23, "executable": { "device": 29, "inode": 31 },
        "cgroup_name": null
    });
    let write = |path: &Path, child: Value| {
        fs::write(
            path,
            serde_json::to_vec(&json!({
                "version": 1, "children": [child]
            }))
            .unwrap(),
        )
        .unwrap();
    };

    // The child publishes readiness independently of the daemon's record write.
    // Even a complete pending record cannot authorize killing the daemon until
    // rename makes that record visible at the path the next boot will read.
    fs::write(root.join("wire-less.ready"), "ready\n").unwrap();
    assert!(!child_is_recorded(&record, 17, &observed));
    let mut previous = child.clone();
    previous["module_id"] = json!("shutdown-observer");
    previous["pid"] = json!(19);
    write(&record, previous);
    write(&pending, child.clone());
    assert!(!child_is_recorded(&record, 17, &observed));
    fs::rename(&pending, &record).unwrap();
    assert!(child_is_recorded(&record, 17, &observed));

    for (field, wrong) in [
        ("pid", json!(19)),
        ("start_time", json!(24)),
        ("start_time", Value::Null),
        ("executable", Value::Null),
        ("executable", json!({ "device": 30, "inode": 31 })),
        ("executable", json!({ "device": 29, "inode": 32 })),
    ] {
        let mut mismatched = child.clone();
        mismatched[field] = wrong;
        write(&record, mismatched);
        assert!(
            !child_is_recorded(&record, 17, &observed),
            "a mismatched or missing {field} cannot identify the orphan"
        );
    }
}

/// Simulated EOF teardown in the observer stub. Long enough that a group kill
/// arriving with the EOF lands mid-teardown; short enough to fit the daemon's
/// child-exit grace.
const OBSERVER_TEARDOWN_MS: u64 = 200;

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
        wait_until_gone(observer, Duration::from_secs(5)),
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
    fixture.term();
    fixture.wait_exit(SHUTDOWN_OBSERVATION_TIMEOUT);
    let sigterms: Vec<_> = fixture
        .events()
        .into_iter()
        .filter(|e| e["kind"] == "sigterm")
        .collect();
    assert_eq!(
        sigterms.len(),
        1,
        "exactly one SIGTERM, at the deadline: {sigterms:?}"
    );
    let term_at = daemon_measurement(&fixture, "sending SIGTERM", "child_stop_elapsed_ms");
    let kill_at = daemon_measurement(&fixture, "sending SIGKILL", "child_stop_elapsed_ms");
    let term_to_kill = daemon_measurement(&fixture, "sending SIGKILL", "term_to_kill_ms");
    // Both signal times use the daemon's child-stop clock. A module's handler
    // and the test's process polling can run much later than either signal.
    assert!(
        term_at >= 1000,
        "SIGTERM came before the module's 1 s deadline: {term_at} ms"
    );
    assert!(
        (500..1500).contains(&term_to_kill),
        "SIGKILL must follow the deadline SIGTERM by 0.5 s: {term_to_kill} ms"
    );
    assert!(kill_at >= term_at + 500);
    let child_stop = daemon_measurement(
        &fixture,
        "supervised child shutdown wait complete",
        "child_stop_elapsed_ms",
    );
    let notice = daemon_measurement(
        &fixture,
        "daemon shutdown forwarding drain complete",
        "notice_elapsed_ms",
    );
    let drain = daemon_measurement(
        &fixture,
        "daemon shutdown forwarding drain complete",
        "drain_elapsed_ms",
    );
    assert!(
        notice + drain + child_stop < 4500,
        "daemon's shutdown waits overran the module's bound: notice={notice}, drain={drain}, children={child_stop} ms"
    );
    assert!(
        wait_until_gone(observer, Duration::from_secs(5)),
        "the module must not outlive the daemon\n{}",
        daemon_log(&fixture)
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
        wait_until_gone(wire_less, Duration::from_secs(5)),
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
        wait_until_gone(wire_less, Duration::from_secs(5)),
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
    // The stub's ready marker only promises an installed SIGTERM handler.
    // It can precede roster admission and the record's atomic rename, so wait
    // for the identity the next boot must read before killing its writer.
    wait_for_recorded_child(
        &fixture.root.join("data/cortexkit/run/live-children.json"),
        orphan as u32,
        Path::new(env!("CARGO_BIN_EXE_fake-aft-stub")),
    );
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
        wait_until_gone(orphan, Duration::from_secs(5)),
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

/// A module treats EOF with no GOODBYE before it as the daemon going away
/// unannounced, so a planned daemon shutdown must send exactly one module
/// GOODBYE, and it must arrive before the connection closes.
#[test]
fn provider_observes_module_goodbye_before_its_eof_at_daemon_shutdown() {
    let mut fixture = Fixture::boot(false);
    fixture.term();
    fixture.wait_exit(Duration::from_secs(4));
    fixture.wait_event("eof");
    let sequence: Vec<_> = fixture
        .events()
        .into_iter()
        .filter_map(|e| e["kind"].as_str().map(str::to_owned))
        .filter(|kind| kind == "draining" || kind == "goodbye" || kind == "eof")
        .collect();
    assert_eq!(
        sequence,
        ["draining", "goodbye", "eof"],
        "module must get one GOODBYE after the drain notice and before EOF"
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
    fixture.term();
    fixture.wait_exit(SHUTDOWN_OBSERVATION_TIMEOUT);
    let drain = daemon_measurement(
        &fixture,
        "daemon shutdown forwarding drain complete",
        "drain_elapsed_ms",
    );
    // A forever-busy gauge must get the two-second forwarding wait, but cannot
    // hold it open until completion. Neither bound includes the EOF teardown.
    assert!(
        (1800..3000).contains(&drain),
        "busy provider's forwarding drain was not bounded at two seconds: {drain} ms\n{}",
        daemon_log(&fixture)
    );
}

#[test]
fn quiescent_provider_does_not_spend_the_whole_drain_budget() {
    let mut fixture = Fixture::boot(false);
    fixture.term();
    fixture.wait_exit(SHUTDOWN_OBSERVATION_TIMEOUT);
    let drain = daemon_measurement(
        &fixture,
        "daemon shutdown forwarding drain complete",
        "drain_elapsed_ms",
    );
    assert!(
        drain < 1500,
        "quiescent provider spent the forwarding drain budget: {drain} ms\n{}",
        daemon_log(&fixture)
    );
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

fn daemon_measurement(fixture: &Fixture, message: &str, field: &str) -> u64 {
    let log = daemon_log(fixture);
    let lines: Vec<_> = log.lines().filter(|line| line.contains(message)).collect();
    assert_eq!(lines.len(), 1, "expected one {message:?} event\n{log}");
    lines[0]
        .split(&format!("{field}="))
        .nth(1)
        .and_then(|tail| tail.split_whitespace().next())
        .and_then(|ms| ms.parse().ok())
        .unwrap_or_else(|| panic!("missing {field} measurement: {}", lines[0]))
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

/// The `capability.requirement` lines in the daemon's log.
fn requirement_lines(fixture: &Fixture) -> Vec<String> {
    daemon_log(fixture)
        .lines()
        .filter(|line| line.contains("capability.requirement"))
        .map(str::to_string)
        .collect()
}

/// During the daemon's own ordered shutdown modules exit one by one, so a
/// provider can deregister before a consumer that requires its capability.
/// The requirement's verdict then becomes `never_provided`, truthfully, but
/// nothing is missing: the stop was planned. That change must be logged at
/// INFO with `cause=daemon_shutdown`, never at ERROR, where operators and
/// monitors counting ERROR lines read it as a fault.
///
/// The order has to be made, not hoped for. At the end of the shutdown the
/// daemon closes every module connection at once, so both modules deregister
/// together however long either process takes to exit, and the verdict then
/// depends on whether the provider's process happened to be reaped yet
/// (`pending` while it still runs, `never_provided` once it is gone). So the
/// consumer holds the shutdown in its drain instead: it reports a busy gauge,
/// which keeps the daemon in the drain for its full budget with the consumer
/// still registered. The provider exits inside that window, and when the
/// drain ends the consumer's deregistration recomputes the requirement with
/// the provider gone.
#[test]
fn required_provider_leaving_during_daemon_shutdown_is_logged_at_info_not_error() {
    const CAPABILITY: &str = "shutdown-test-identity/v1";
    let permit = DAEMON_GATE.lock().unwrap_or_else(|p| p.into_inner());
    let root = fresh_dir("requirement-shutdown");
    for dir in ["config/cortexkit", "runtime", "data/cortexkit/run"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    let stub = env!("CARGO_BIN_EXE_fake-aft-stub");
    let modules = json!({
        // No SIGTERM handler: the test's signal during the drain ends it.
        "requirement-provider": {
            "program": stub,
            "env": {
                "FAKE_AFT_MODULE_ID": "requirement-provider",
                "FAKE_AFT_PID_PATH": root.join("provider.pid"),
                "FAKE_AFT_CAPABILITIES": json!({ "provides": [CAPABILITY] }).to_string(),
            }
        },
        // Busy, so the shutdown's drain waits its whole budget on it.
        "requirement-consumer": {
            "program": stub,
            "env": {
                "FAKE_AFT_MODULE_ID": "requirement-consumer",
                "FAKE_AFT_PID_PATH": root.join("consumer.pid"),
                "FAKE_AFT_EVENTS_PATH": root.join("events.jsonl"),
                "FAKE_AFT_BUSY_GAUGES": "work",
                "FAKE_AFT_HEALTH_METRICS": "{\"work\":1}",
                "FAKE_AFT_CAPABILITIES": json!({
                    "requires": [{ "capability": CAPABILITY, "need": "required" }]
                }).to_string(),
            }
        },
    });
    fs::write(
        root.join("config/cortexkit/subc.jsonc"),
        serde_json::to_vec(&json!({ "version": 1, "modules": modules })).unwrap(),
    )
    .unwrap();
    let child = spawn_daemon(&root);
    let mut fixture = Fixture {
        root,
        child,
        _permit: permit,
    };

    let is_consumer_line = |line: &&String| {
        line.contains("consumer=requirement-consumer") && line.contains(CAPABILITY)
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !requirement_lines(&fixture)
        .iter()
        .filter(is_consumer_line)
        .any(|line| line.contains("verdict=provided"))
    {
        assert!(
            fixture.child.try_wait().unwrap().is_none(),
            "daemon exited during startup"
        );
        assert!(
            Instant::now() < deadline,
            "the requirement never read provided:\n{}",
            daemon_log(&fixture)
        );
        thread::sleep(Duration::from_millis(10));
    }
    let provider = fixture.pid_of("provider.pid");
    let consumer = fixture.pid_of("consumer.pid");

    fixture.term();
    // The drain notice follows the daemon-shutdown flag, so from here on the
    // daemon is in its announced shutdown.
    fixture.wait_event("draining");
    signal_pid(provider, rustix::process::Signal::TERM);
    // Gone means reaped by the daemon, not merely dead: a zombie still counts.
    assert!(
        wait_until_gone(provider, Duration::from_secs(2)),
        "the provider did not exit during the drain"
    );
    assert!(
        fixture.child.try_wait().unwrap().is_none() && process_alive(consumer),
        "the consumer must still be holding the shutdown when the provider has gone"
    );
    fixture.wait_exit(Duration::from_secs(8));

    let log = daemon_log(&fixture);
    let provider_exit = log.lines().position(|line| {
        line.contains("exited during daemon shutdown")
            && line.contains("module_id=requirement-provider")
    });
    let consumer_deregistered = log.lines().position(|line| {
        line.contains("module registration ended")
            && line.contains("module_id=requirement-consumer")
    });
    assert!(
        matches!((provider_exit, consumer_deregistered), (Some(exit), Some(ended)) if exit < ended),
        "the provider must have exited while the consumer was still registered:\n{log}"
    );

    let lines = requirement_lines(&fixture);
    let never_provided: Vec<&String> = lines
        .iter()
        .filter(is_consumer_line)
        .filter(|line| line.contains("verdict=never_provided"))
        .collect();
    assert!(
        !never_provided.is_empty(),
        "the provider's departure must still change the verdict to never_provided:\n{log}"
    );
    for line in &never_provided {
        assert!(
            line.contains(" INFO ") && line.contains("cause=daemon_shutdown"),
            "a requirement change during the shutdown must log at INFO with cause=daemon_shutdown: {line}"
        );
    }
    let errors: Vec<&String> = lines
        .iter()
        .filter(|line| line.contains(" ERROR "))
        .collect();
    assert!(
        errors.is_empty(),
        "no capability.requirement line may log at ERROR during a planned shutdown: {errors:?}"
    );
}

#[test]
fn second_sigterm_cuts_short_a_stubborn_provider_wait() {
    let mut fixture = Fixture::boot_with(
        true,
        json!({
            "drain_timeout_ms": 10000,
            "env": { "FAKE_AFT_IGNORE_EOF": "1" }
        }),
        None,
    );
    fixture.term();
    fixture.wait_event("draining");
    assert!(fixture.child.try_wait().unwrap().is_none());
    fixture.term();
    fixture.wait_exit(SHUTDOWN_OBSERVATION_TIMEOUT);
    let elapsed = daemon_measurement(
        &fixture,
        "daemon shutdown wait ended after second SIGTERM",
        "second_sigterm_elapsed_ms",
    );
    assert!(
        elapsed < 800,
        "second SIGTERM did not cut short the daemon's waits: {elapsed} ms\n{}",
        daemon_log(&fixture)
    );
    assert!(daemon_log(&fixture)
        .lines()
        .any(|line| line.contains("sending SIGKILL") && line.contains("escalated=true")));
    assert!(
        wait_until_gone(fixture.pid_of("observer.pid"), Duration::from_secs(5)),
        "second SIGTERM left the module running"
    );
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
    tokio::time::timeout(SHUTDOWN_OBSERVATION_TIMEOUT, async {
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
    .unwrap_or_else(|error| {
        panic!(
            "waiting for route.closing and EOF: {error}\n{}",
            daemon_log(&fixture)
        )
    });
    assert_eq!(sequence, ["route.closing", "eof"]);
    fixture.wait_exit(SHUTDOWN_OBSERVATION_TIMEOUT);
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
            br#"{"respond_on_draining":true}"#.to_vec(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    fixture.wait_event("request_received");
    fixture.term();
    let mut sequence = Vec::new();
    // The module holds this accepted request until the daemon's draining
    // notice, so the reply cannot accidentally finish before shutdown begins.
    tokio::time::timeout(SHUTDOWN_OBSERVATION_TIMEOUT, async {
        while let Some(frame) = read_frame(&mut stream).await.unwrap() {
            if frame.header.ty == FrameType::Response && frame.header.corr == 2 {
                sequence.push("reply");
            }
        }
        sequence.push("eof");
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "waiting for in-flight reply and EOF: {error}\n{}",
            daemon_log(&fixture)
        )
    });
    assert_eq!(sequence, ["reply", "eof"]);
    fixture.wait_exit(SHUTDOWN_OBSERVATION_TIMEOUT);
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
    let mut child = dev_command(common::ckdev_subc())
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
    let output = dev_command(common::ckdev_subc())
        .env("SUBC_CGROUP_PLACEMENT", "current")
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("XDG_CONFIG_HOME", root.join("config"))
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
