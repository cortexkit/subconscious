use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Condvar, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use subc_daemon::test_support::TestTempDir;

/// At most two of this file's daemons run at once.
///
/// Each test here spawns a REAL `ck-subc`, and the file has seven. The harness
/// runs tests in parallel, so unlimited they add seven concurrent daemons to a
/// `--workspace` run that is already spawning daemons in subc-client-rs.
/// Measured on the merged tree: one full run gave 1395/0, the next a
/// registration timeout in `subc-client-rs/tests/real_daemon.rs` -- a DIFFERENT
/// test each time, always "module did not register in catalog within 10s",
/// never a failure of anything this file asserts.
///
/// The failure was OUR load landing on someone else's bound, so the fix belongs
/// here rather than in their timeout: a bound widened to survive whatever load
/// arrives next stops meaning anything, and the load is ours to cap.
fn daemon_gate() -> &'static (Mutex<usize>, Condvar) {
    static GATE: OnceLock<(Mutex<usize>, Condvar)> = OnceLock::new();
    GATE.get_or_init(|| (Mutex::new(0usize), Condvar::new()))
}

struct Fixture {
    root: TestTempDir,
    child: Option<Child>,
    holds_permit: bool,
}

impl Fixture {
    fn new() -> Self {
        let root = TestTempDir::new("durable-terminals");
        fs::create_dir_all(root.join("config/cortexkit")).unwrap();
        fs::create_dir_all(root.join("runtime")).unwrap();
        fs::create_dir_all(root.join("data/cortexkit/run")).unwrap();
        fs::write(
            root.join("config/cortexkit/subc.jsonc"),
            serde_json::to_vec(&json!({
                "version": 1,
                "modules": {
                    "history": {
                        "program": env!("CARGO_BIN_EXE_fake-aft-stub"),
                        "enabled": false,
                        "drain_timeout_ms": 25,
                        "env": { "FAKE_AFT_MODULE_ID": "history" }
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        Self {
            root,
            child: None,
            holds_permit: false,
        }
    }

    fn journal(&self) -> PathBuf {
        self.root.join("data/cortexkit/run/terminals.jsonl")
    }

    fn connection(&self) -> PathBuf {
        self.root
            .join("runtime")
            .join(subc_transport::CONNECTION_FILE_NAME)
    }

    fn acquire_permit(&mut self) {
        if !self.holds_permit {
            let (lock, cvar) = daemon_gate();
            let mut live = lock.lock().unwrap_or_else(|p| p.into_inner());
            while *live >= 2 {
                live = cvar.wait(live).unwrap_or_else(|p| p.into_inner());
            }
            *live += 1;
            self.holds_permit = true;
        }
    }

    fn boot(&mut self) {
        self.acquire_permit();
        self.child = Some(
            Command::new(env!("CARGO_BIN_EXE_ck-subc"))
                .env("XDG_DATA_HOME", self.root.join("data"))
                .env("XDG_CONFIG_HOME", self.root.join("config"))
                .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
                .env("SUBC_PORT", "0")
                .env("SUBC_CGROUP_PLACEMENT", "disabled")
                .env_remove("CK_LOG")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                panic!("daemon did not boot: {status}");
            }
            if self.connection().exists()
                && self
                    .try_ck(&["module", "terminals", "history", "--json"])
                    .is_some()
            {
                break;
            }
            if Instant::now() >= deadline {
                panic!("daemon never became readable");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_file(self.connection());
        if self.holds_permit {
            let (lock, cvar) = daemon_gate();
            let mut live = lock.lock().unwrap_or_else(|p| p.into_inner());
            *live = live.saturating_sub(1);
            cvar.notify_one();
            self.holds_permit = false;
        }
    }

    fn try_ck(&self, args: &[&str]) -> Option<String> {
        try_ck_at(&self.connection(), args)
    }

    fn terminals(&self) -> Value {
        ck_at(
            &self.connection(),
            &["module", "terminals", "history", "--json"],
        )
    }

    fn exit(&self) {
        exit_history_module(&self.connection());
    }
}

fn try_ck_at(connection: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new(env!("CARGO_BIN_EXE_ck"))
        .arg("--subc")
        .arg(connection)
        .args(args)
        .output()
        .unwrap();
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).unwrap())
}

fn ck_at(connection: &Path, args: &[&str]) -> Value {
    serde_json::from_str(&try_ck_at(connection, args).expect("ck command succeeds")).unwrap()
}

/// Start the `history` module, wait until it registers, then stop it, which
/// leaves exactly one terminal exit behind.
fn exit_history_module(connection: &Path) {
    ck_at(connection, &["module", "start", "history", "--json"]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while ck_at(connection, &["module", "status", "history", "--json"])["module"]["live"] != true {
        if Instant::now() >= deadline {
            panic!("module never registered");
        }
        thread::sleep(Duration::from_millis(10));
    }
    ck_at(connection, &["module", "stop", "history", "--json"]);
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.kill();
    }
}

#[test]
fn daemon_restart_recovers_terminal_when_the_ring_is_empty() {
    let mut fixture = Fixture::new();
    fixture.boot();
    fixture.exit();
    let before = fixture.terminals()["entries"].as_array().unwrap().clone();
    fixture.kill();
    fixture.boot();
    let after = fixture.terminals()["entries"].as_array().unwrap().clone();
    assert_eq!((before.len(), after), (1, before));
}

#[test]
fn daemon_restart_distinguishes_incarnations_on_both_sides() {
    let mut fixture = Fixture::new();
    fixture.boot();
    fixture.exit();
    fixture.kill();
    fixture.boot();
    fixture.exit();
    let history = fixture.terminals();
    let entries = history["entries"].as_array().unwrap();
    let incarnations = entries
        .iter()
        .map(|entry| entry["daemon_incarnation"].as_str().unwrap_or(""))
        .collect::<Vec<_>>();
    assert!(
        incarnations.len() == 2
            && !incarnations[0].is_empty()
            && !incarnations[1].is_empty()
            && incarnations[0] != incarnations[1],
        "distinct daemon lifetimes must remain distinguishable: {incarnations:?}"
    );
}

#[test]
fn daemon_boots_with_missing_terminal_journal() {
    let mut fixture = Fixture::new();
    fixture.boot();
    assert_eq!(fixture.terminals()["entries"], json!([]));
}

#[test]
fn daemon_boots_with_empty_terminal_journal() {
    let mut fixture = Fixture::new();
    fs::write(fixture.journal(), b"").unwrap();
    fixture.boot();
    assert_eq!(fixture.terminals()["entries"], json!([]));
}

#[test]
fn daemon_boots_with_unreadable_terminal_journal_and_reports_it() {
    let mut fixture = Fixture::new();
    // A directory is unreadable as a journal on Unix and Windows, including
    // privileged test runners for whom mode 000 would still be readable.
    fs::create_dir(fixture.journal()).unwrap();
    fixture.boot();
    let history = fixture.terminals();
    assert_eq!(
        (
            history["entries"].clone(),
            history["journal_read_errors"].clone()
        ),
        (json!([]), json!(1))
    );
}

#[test]
fn daemon_boots_with_garbage_terminal_journal_and_reports_skipped_lines() {
    let mut fixture = Fixture::new();
    fs::write(fixture.journal(), b"garbage\n\xff\n{\"partial\":").unwrap();
    fixture.boot();
    let history = fixture.terminals();
    assert_eq!(
        (
            history["entries"].clone(),
            history["journal_skipped_lines"].clone()
        ),
        (json!([]), json!(3))
    );
}

#[test]
fn cli_renders_the_incarnation_and_corruption_warning() {
    let mut fixture = Fixture::new();
    fs::write(fixture.journal(), b"bad line\n").unwrap();
    fixture.boot();
    fixture.exit();
    let history = fixture.terminals();
    let incarnation = history["entries"][0]["daemon_incarnation"]
        .as_str()
        .unwrap();
    let text = fixture.try_ck(&["module", "terminals", "history"]).unwrap();
    assert!(
        text.contains(incarnation)
            && text.contains("warning: 1 unreadable terminal journal lines skipped"),
        "{text}"
    );
}

/// Set on the re-executed test binary: the fixture root the child daemon uses.
const NO_JOURNAL_CHILD_ROOT_ENV: &str = "SUBC_TEST_NO_JOURNAL_CHILD_ROOT";

/// An in-process daemon (`run_with_config`, the entry point sibling repos'
/// test harnesses use) that leaves `terminal_journal_path` unset must journal
/// NOWHERE -- in particular not into `<data home>/cortexkit/run/terminals.jsonl`,
/// which is the operator's journal that `ck module terminals` reads.
///
/// The daemon runs in a re-executed copy of this test binary so its data home
/// can be an isolated `XDG_DATA_HOME` without mutating this process's
/// environment. The assertion is on that isolated tree, which is exactly where
/// a fallback to the ambient run directory would write: nothing else in the
/// tree can produce a `terminals.jsonl`.
#[test]
fn an_in_process_daemon_without_a_journal_path_writes_no_journal() {
    let mut fixture = Fixture::new();
    fixture.acquire_permit();
    let root = fixture.root.path().to_path_buf();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "in_process_daemon_without_a_journal_path_child",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(NO_JOURNAL_CHILD_ROOT_ENV, &root)
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("HOME", root.join("home"))
        .env_remove("SUBC_CONNECTION_FILE")
        .env_remove("CK_LOG")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "the in-process child daemon must run and pass exactly one test\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let journals = files_named(&root, "terminals.jsonl");
    assert!(
        journals.is_empty(),
        "an in-process daemon with no journal path wrote a terminal journal: {journals:?}"
    );
}

/// The daemon half of the test above. Does nothing unless re-executed by it.
#[test]
fn in_process_daemon_without_a_journal_path_child() {
    let Some(root) = std::env::var_os(NO_JOURNAL_CHILD_ROOT_ENV) else {
        return;
    };
    let root = PathBuf::from(root);
    let connection = root
        .join("runtime")
        .join(subc_transport::CONNECTION_FILE_NAME);
    let config = subc_daemon::bootstrap::BootstrapConfig::new(&connection, 0)
        .with_daemon_config_path(root.join("config/cortexkit/subc.jsonc"))
        .unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let daemon = runtime.spawn(subc_daemon::bootstrap::run_with_config(config));

    let deadline = Instant::now() + Duration::from_secs(10);
    while !(connection.exists()
        && try_ck_at(&connection, &["module", "terminals", "history", "--json"]).is_some())
    {
        assert!(
            !daemon.is_finished(),
            "in-process daemon exited during boot"
        );
        assert!(Instant::now() < deadline, "daemon never became readable");
        thread::sleep(Duration::from_millis(10));
    }

    exit_history_module(&connection);
    // The exit reaches the ring only after the journal append would have run
    // (both happen under one lock), so once it is visible here any journal
    // write has already happened.
    let deadline = Instant::now() + Duration::from_secs(10);
    let history = loop {
        let history = ck_at(&connection, &["module", "terminals", "history", "--json"]);
        if history["entries"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty())
        {
            break history;
        }
        assert!(Instant::now() < deadline, "the exit never reached the ring");
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        (
            history["entries"].as_array().unwrap().len(),
            // Zero counters are omitted from the wire, so absent reads as 0.
            history["journal_skipped_lines"].as_u64().unwrap_or(0),
            history["journal_read_errors"].as_u64().unwrap_or(0),
            history["journal_write_failures"].as_u64().unwrap_or(0),
        ),
        (1, 0, 0, 0),
        "the ring must answer with the exit and zero journal counters: {history}"
    );
    daemon.abort();
    runtime.shutdown_timeout(Duration::from_secs(2));
    let _ = fs::remove_file(&connection);
}

fn files_named(root: &Path, name: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                pending.push(path);
            } else if entry.file_name() == name {
                found.push(path);
            }
        }
    }
    found
}

/// With no absolute data home, the daemon binary must refuse to start and say
/// which variables to set, rather than resolving its run directory (logs,
/// terminal journal) under the directory it was started from.
#[test]
fn the_daemon_binary_refuses_a_relative_data_home_by_name() {
    let root = TestTempDir::new("relative-data-home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_ck-subc"));
    command
        .current_dir(root.path())
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("SUBC_PORT", "0")
        .env("SUBC_CGROUP_PLACEMENT", "disabled")
        .env_remove("XDG_DATA_HOME")
        .env_remove("HOME")
        .env_remove("APPDATA")
        .env_remove("USERPROFILE")
        .env_remove("CK_LOG")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("ck-subc started with a relative data home instead of refusing");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
    assert!(
        !status.success() && stderr.contains("XDG_DATA_HOME") && stderr.contains("HOME"),
        "ck-subc must fail naming the variables to set; status {status}, stderr: {stderr}"
    );
    assert!(
        !root.join(".local").exists(),
        "a refused start must not create a run directory under the working directory"
    );
}
