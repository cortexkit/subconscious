#![cfg(unix)]

//! A daemon owns its run directory for its whole life, and a second daemon
//! sharing that directory refuses to start instead of acting on it.
//!
//! The singleton check is keyed on the connection file in the runtime
//! directory, while the run directory (and the live-children record the
//! boot-time orphan sweep reads) is derived from the data home. A daemon
//! started with another runtime directory over the same data home used to
//! pass the singleton check, read the running daemon's record as a crashed
//! daemon's, and SIGTERM every module the running daemon supervised.

use std::{
    fs,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use subc_test_support::{process_alive, wait_until_gone, TestTempDir};

// Real daemons compete with other integration binaries for spawn and
// registration resources; serializing this file keeps the deadlines honest.
static DAEMON_GATE: Mutex<()> = Mutex::new(());

const MODULE_ID: &str = "wire-less";

/// One scratch tree: a data home and config home shared by every daemon in
/// the test, and one runtime directory per name handed to `spawn_daemon`.
struct Tree {
    root: TestTempDir,
    daemons: Vec<Child>,
    _permit: MutexGuard<'static, ()>,
}

impl Tree {
    fn new(label: &str) -> Self {
        let permit = DAEMON_GATE.lock().unwrap_or_else(|p| p.into_inner());
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = TestTempDir::new(&format!("{label}-{nanos}"));
        assert!(
            fs::read_dir(&*root).unwrap().next().is_none(),
            "scratch directory {} already has contents",
            root.display()
        );
        for dir in ["config/cortexkit", "data/cortexkit/run"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        // A `protocol: "none"` module: it outlives a SIGKILLed daemon (modules
        // lead their own process groups), and it writes a marker when it is
        // sent SIGTERM, which is exactly what the orphan sweep does.
        let modules = json!({ MODULE_ID: {
            "program": env!("CARGO_BIN_EXE_fake-aft-stub"),
            "protocol": "none",
            "env": {
                "FAKE_AFT_NEVER_CONNECT": "1",
                "FAKE_AFT_PID_PATH": root.join("module.pid"),
                "FAKE_AFT_NEVER_CONNECT_READY_PATH": root.join("module.ready"),
                "FAKE_AFT_SIGTERM_MARKER_PATH": root.join("module.sigterm"),
            },
        }});
        fs::write(
            root.join("config/cortexkit/subc.jsonc"),
            serde_json::to_vec(&json!({ "version": 1, "modules": modules })).unwrap(),
        )
        .unwrap();
        Self {
            root,
            daemons: Vec::new(),
            _permit: permit,
        }
    }

    fn run_dir(&self) -> PathBuf {
        self.root.join("data/cortexkit/run")
    }

    fn runtime_dir(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// Start the shipped daemon binary with every per-user location inside
    /// this tree: the shared data and config homes and the named runtime
    /// directory. The environment is set on the child only, never on the
    /// test process. Returns the index into `daemons`.
    fn spawn_daemon(&mut self, runtime: &str) -> usize {
        let runtime_dir = self.runtime_dir(runtime);
        fs::create_dir_all(&runtime_dir).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_ck-subc"))
            .process_group(0)
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_RUNTIME_DIR", runtime_dir)
            .env("SUBC_PORT", "0")
            .env("SUBC_CGROUP_PLACEMENT", "disabled")
            .env_remove("CK_LOG")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        self.daemons.push(child);
        self.daemons.len() - 1
    }

    /// Wait until the module is parked with its SIGTERM handler installed and
    /// the daemon has recorded it; returns its pid.
    fn wait_module_ready(&mut self, daemon: usize) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                self.daemons[daemon].try_wait().unwrap().is_none(),
                "daemon exited during startup: {}",
                self.stderr_of(daemon)
            );
            if self.root.join("module.ready").exists() {
                if let Some(pid) = self.module_pid() {
                    if self.recorded_pids().contains(&pid) {
                        return pid;
                    }
                }
            }
            assert!(Instant::now() < deadline, "module never became ready");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn module_pid(&self) -> Option<i32> {
        fs::read_to_string(self.root.join("module.pid"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// The pids the live-children record in the shared run directory lists.
    fn recorded_pids(&self) -> Vec<i32> {
        let Ok(bytes) = fs::read(self.run_dir().join("live-children.json")) else {
            return Vec::new();
        };
        let record: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        record["children"]
            .as_array()
            .map(|children| {
                children
                    .iter()
                    .filter_map(|child| child["pid"].as_i64())
                    .map(|pid| pid as i32)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Wait up to `budget` for a daemon to exit; `None` if it is still running.
    fn wait_exit(&mut self, daemon: usize, budget: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + budget;
        loop {
            if let Some(status) = self.daemons[daemon].try_wait().unwrap() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Everything the daemon wrote to stderr. Only meaningful once it exited.
    fn stderr_of(&mut self, daemon: usize) -> String {
        use std::io::Read;
        let mut text = String::new();
        if self.daemons[daemon].try_wait().unwrap().is_some() {
            if let Some(stderr) = self.daemons[daemon].stderr.as_mut() {
                let _ = stderr.read_to_string(&mut text);
            }
        }
        text
    }

    /// SIGKILL a daemon's process group, as a crash or a service manager would.
    fn kill_daemon(&mut self, daemon: usize) {
        let _ = rustix::process::kill_process_group(
            rustix::process::Pid::from_raw(self.daemons[daemon].id() as i32).unwrap(),
            rustix::process::Signal::KILL,
        );
        self.daemons[daemon].wait().unwrap();
    }

    fn daemon_log(&self) -> String {
        let mut files: Vec<PathBuf> = fs::read_dir(self.run_dir().join("logs"))
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
}

impl Drop for Tree {
    fn drop(&mut self) {
        for daemon in &mut self.daemons {
            let _ = rustix::process::kill_process_group(
                rustix::process::Pid::from_raw(daemon.id() as i32).unwrap(),
                rustix::process::Signal::KILL,
            );
            let _ = daemon.wait();
        }
        // The module leads its own process group, so a daemon kill does not
        // reach it; it must not outlive the test.
        if let Some(pid) = self.module_pid().and_then(rustix::process::Pid::from_raw) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
    }
}

fn lock_path(run_dir: &Path) -> PathBuf {
    run_dir.join("daemon.lock")
}

/// Daemon B shares daemon A's data home (so its run directory
/// and live-children record) but has its own runtime directory (so its own
/// connection file and start lock). B must refuse to start, naming the lock
/// and A's pid, and leave A's module and record untouched.
#[test]
fn a_second_daemon_over_the_same_data_home_refuses_and_leaves_the_owners_modules_alone() {
    let mut tree = Tree::new("run-dir-shared-data-home");
    let a = tree.spawn_daemon("runtime-a");
    let module = tree.wait_module_ready(a);
    let a_pid = tree.daemons[a].id();

    let b = tree.spawn_daemon("runtime-b");
    // Long enough for an unguarded B to finish its sweep (the module exits on
    // its first SIGTERM), so a missing lock shows up as a killed module below
    // rather than only as a B that did not exit.
    let status = tree.wait_exit(b, Duration::from_secs(5));

    assert!(
        !tree.root.join("module.sigterm").exists(),
        "daemon B signalled daemon A's module"
    );
    assert!(
        process_alive(module),
        "daemon A's module (pid {module}) is gone"
    );
    assert_eq!(
        tree.module_pid(),
        Some(module),
        "daemon A's module was replaced"
    );
    assert!(
        tree.recorded_pids().contains(&module),
        "daemon A's live-children record no longer lists its module: {:?}",
        tree.recorded_pids()
    );
    let status = status.expect("daemon B must refuse to start, not keep running");
    assert!(!status.success(), "daemon B must exit nonzero: {status}");
    let stderr = tree.stderr_of(b);
    let lock = lock_path(&tree.run_dir());
    assert!(
        stderr.contains(&format!("run directory lock {}", lock.display())),
        "the refusal must name the lock: {stderr}"
    );
    assert!(
        stderr.contains(&format!("(pid {a_pid})")),
        "the refusal must name the holder's pid {a_pid}: {stderr}"
    );
    assert!(
        tree.daemons[a].try_wait().unwrap().is_none(),
        "daemon A must still be running"
    );
}

/// A second start sharing both the runtime directory and the data home is
/// the ordinary "already running" case: it exits 0 before touching the run
/// directory lock.
#[test]
fn a_second_start_over_the_same_runtime_dir_still_reports_already_running() {
    let mut tree = Tree::new("run-dir-same-runtime");
    let a = tree.spawn_daemon("runtime");
    let module = tree.wait_module_ready(a);

    let b = tree.spawn_daemon("runtime");
    let status = tree
        .wait_exit(b, Duration::from_secs(10))
        .expect("the second start must exit");
    assert!(
        status.success(),
        "already running is a successful start: {status}: {}",
        tree.stderr_of(b)
    );
    assert!(
        tree.daemon_log().contains("subc daemon already running"),
        "the second start must report the running daemon"
    );
    assert!(process_alive(module), "the running daemon's module is gone");
    assert!(tree.daemons[a].try_wait().unwrap().is_none());
}

/// The lock is released by the kernel when its holder dies, so a SIGKILLed
/// daemon does not block the next one, and the next one's orphan sweep still
/// ends the module the dead daemon left behind.
#[test]
fn a_killed_daemons_lock_does_not_block_the_next_daemon_and_its_orphan_is_swept() {
    let mut tree = Tree::new("run-dir-after-crash");
    let a = tree.spawn_daemon("runtime");
    let orphan = tree.wait_module_ready(a);
    tree.kill_daemon(a);
    assert!(
        process_alive(orphan),
        "precondition: a protocol none module outlives a SIGKILLed daemon"
    );
    for file in ["module.pid", "module.ready"] {
        fs::remove_file(tree.root.join(file)).unwrap();
    }

    let c = tree.spawn_daemon("runtime");
    let replacement = tree.wait_module_ready(c);
    assert_ne!(replacement, orphan, "the module was spawned again");
    // The sweep spawns the replacement once the orphan has exited (it no longer
    // matches the pid, start time and executable the previous daemon recorded).
    // Reaping the exited orphan is left to whichever process adopted it, so
    // allow it a moment to disappear.
    assert!(
        wait_until_gone(orphan, Duration::from_secs(5)),
        "the next daemon must end the previous daemon's orphan"
    );
    assert_eq!(
        fs::read_to_string(tree.root.join("module.sigterm"))
            .ok()
            .as_deref(),
        Some("sigterm\n"),
        "the orphan must be asked to stop with SIGTERM"
    );
    assert_eq!(
        fs::read_to_string(lock_path(&tree.run_dir()))
            .unwrap()
            .trim(),
        tree.daemons[c].id().to_string(),
        "the next daemon must hold the run directory lock"
    );
    assert!(
        tree.daemon_log()
            .contains("orphan sweep: previous daemon's child exited after SIGTERM"),
        "the next daemon's sweep did not run"
    );
}

#[test]
fn scope_sync_stub_exits_when_daemon_is_sigkilled() {
    let mut tree = Tree::new("scope-sync-daemon-eof");
    let scopes = tree.root.join("scopes.json");
    let events = tree.root.join("events.jsonl");
    let nonce = tree.root.join("nonce");
    fs::write(&scopes, "[]").unwrap();
    fs::write(
        tree.root.join("config/cortexkit/subc.jsonc"),
        serde_json::to_vec(&json!({ "version": 1, "modules": { "aft": {
            "program": env!("CARGO_BIN_EXE_fake-aft-stub"),
            "env": {
                "FAKE_AFT_PID_PATH": tree.root.join("module.pid"),
                "FAKE_AFT_SCOPE_SYNC_PATH": scopes,
                "FAKE_AFT_LAUNCH_NONCE_PATH": nonce,
                "FAKE_AFT_EVENTS_PATH": events,
                // The readiness watcher waits for a file that never appears,
                // so EOF must stop it without waiting for its sender to drop.
                "FAKE_AFT_READY_UPDATE_PATH": tree.root.join("never-ready"),
            },
        }}}))
        .unwrap(),
    )
    .unwrap();
    let daemon = tree.spawn_daemon("runtime");
    let deadline = Instant::now() + Duration::from_secs(10);
    let pid = loop {
        assert!(
            tree.daemons[daemon].try_wait().unwrap().is_none(),
            "daemon exited during startup"
        );
        let sent = fs::read_to_string(&events)
            .unwrap_or_default()
            .contains("scope_sync_sent");
        if sent && nonce.exists() {
            if let Some(pid) = tree.module_pid() {
                assert!(process_alive(pid), "stub exited before daemon kill");
                break pid;
            }
        }
        assert!(Instant::now() < deadline, "stub never sent scope sync");
        thread::sleep(Duration::from_millis(10));
    };
    tree.kill_daemon(daemon);
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "scope-sync stub {pid} survived daemon SIGKILL"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
