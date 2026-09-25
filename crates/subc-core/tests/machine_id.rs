//! The daemon's machine id, through the shipped daemon binary and the ck CLI.
//!
//! Every daemon and every ck here runs with XDG_DATA_HOME, XDG_CONFIG_HOME and
//! XDG_RUNTIME_DIR inside the test's own temporary tree. The machine id lives
//! under the data home, so a missed isolation would mint into, or adopt over,
//! the operator's real one.

use std::{
    fs,
    io::Read,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::{Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use subc_test_support::TestTempDir;

mod common;

const START_TIMEOUT: Duration = Duration::from_secs(20);
const ADOPTED: &str = "fedcba9876543210fedcba9876543210";

// Real daemons compete with the other integration binaries for spawn and
// startup time; one at a time keeps the startup deadline meaningful.
static DAEMON_GATE: Mutex<()> = Mutex::new(());

struct Home {
    root: TestTempDir,
    _permit: MutexGuard<'static, ()>,
}

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Home {
    fn new(label: &str) -> Self {
        let permit = DAEMON_GATE.lock().unwrap_or_else(|p| p.into_inner());
        let root = TestTempDir::new(label);
        for dir in ["data", "config", "runtime"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        Self {
            root,
            _permit: permit,
        }
    }

    fn machine_id_file(&self) -> PathBuf {
        self.root.join("data").join("cortexkit").join("machine-id")
    }

    fn connection_file(&self) -> PathBuf {
        self.root
            .join("runtime")
            .join(subc_transport::CONNECTION_FILE_NAME)
    }

    fn isolate<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        command
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("SUBC_PORT", "0")
            .env_remove(subc_protocol::SUBC_MODULE_ID_ENV)
            .env_remove(subc_protocol::SUBC_LAUNCH_NONCE_ENV)
            .env_remove("CK_LOG")
    }

    fn spawn_daemon(&self) -> Child {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ck-subc"));
        command.env("SUBC_CGROUP_PLACEMENT", "disabled");
        self.isolate(&mut command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// Boot a daemon and wait until `ck daemon --json` answers through it.
    fn boot(&self) -> Daemon {
        let mut daemon = Daemon {
            child: self.spawn_daemon(),
        };
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if let Some(status) = daemon.child.try_wait().unwrap() {
                let mut stderr = String::new();
                if let Some(mut pipe) = daemon.child.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                panic!("daemon exited during startup ({status}): {stderr}");
            }
            if self.connection_file().exists() && self.ck(&["daemon", "--json"]).status.success() {
                return daemon;
            }
            assert!(Instant::now() < deadline, "daemon did not start");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn stop(&self, daemon: Daemon) {
        drop(daemon);
        // The killed daemon leaves its connection file; remove it so a later
        // `ck` in this test cannot mistake the stale file for a live daemon.
        let _ = fs::remove_file(self.connection_file());
    }

    fn ck(&self, args: &[&str]) -> Output {
        let mut command = common::ck_under_test_command();
        self.isolate(&mut command)
            .arg("--subc")
            .arg(self.connection_file())
            .args(args)
            .output()
            .unwrap()
    }

    fn served_machine_id(&self) -> Option<String> {
        let output = self.ck(&["daemon", "--json"]);
        assert!(output.status.success(), "{}", text(&output.stderr));
        let describe: Value = serde_json::from_slice(&output.stdout).unwrap();
        describe["machine_id"].as_str().map(str::to_owned)
    }

    fn stored(&self) -> String {
        fs::read_to_string(self.machine_id_file()).unwrap()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn assert_machine_id_shape(id: &str) {
    assert_eq!(id.len(), 32, "{id:?}");
    assert!(
        id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "{id:?}"
    );
}

#[test]
fn a_second_boot_with_the_same_data_home_serves_the_same_machine_id() {
    let home = Home::new("machine-id-second-boot");
    assert!(!home.machine_id_file().exists());

    let first = home.boot();
    let stored = home.stored();
    let minted = stored.strip_suffix('\n').expect("one line").to_string();
    assert_machine_id_shape(&minted);
    assert_eq!(home.served_machine_id().as_deref(), Some(minted.as_str()));
    // `ck daemon` prints it for a human too.
    let rendered = home.ck(&["daemon"]);
    assert!(
        text(&rendered.stdout).contains(&format!("machine id: {minted}\n")),
        "{}",
        text(&rendered.stdout)
    );
    home.stop(first);

    let _second = home.boot();
    assert_eq!(
        home.served_machine_id().as_deref(),
        Some(minted.as_str()),
        "a second boot over the same data home must serve the id the first one minted"
    );
    assert_eq!(home.stored(), stored, "the daemon never rewrites the file");
}

#[test]
fn a_corrupt_machine_id_file_stops_boot_by_name_and_is_left_untouched() {
    let home = Home::new("machine-id-corrupt");
    fs::create_dir_all(home.machine_id_file().parent().unwrap()).unwrap();
    let corrupt = "0123456789ABCDEF0123456789abcdef\n";
    fs::write(home.machine_id_file(), corrupt).unwrap();

    let mut child = home.spawn_daemon();
    let deadline = Instant::now() + START_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("a daemon over a corrupt machine id kept running");
        }
        thread::sleep(Duration::from_millis(20));
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();

    assert!(!status.success(), "boot must fail: {stderr}");
    assert!(
        stderr.contains("machine id file")
            && stderr.contains("corrupt")
            && stderr.contains(&home.machine_id_file().display().to_string()),
        "the error names the machine id file: {stderr}"
    );
    assert_eq!(home.stored(), corrupt, "the corrupt file is left as it was");
    assert!(
        !home.connection_file().exists(),
        "nothing is published before the machine id is established"
    );
}

#[test]
fn ck_machine_adopt_writes_the_file_and_refuses_a_malformed_id() {
    let home = Home::new("machine-id-adopt");

    for malformed in ["FEDCBA9876543210FEDCBA9876543210", "fedcba98", "not-an-id"] {
        let refused = home.ck(&["machine", "adopt", malformed]);
        assert!(!refused.status.success(), "{malformed} was accepted");
        assert!(
            text(&refused.stderr).contains("is not a machine id"),
            "{}",
            text(&refused.stderr)
        );
        assert!(!home.machine_id_file().exists());
    }

    let adopted = home.ck(&["machine", "adopt", ADOPTED]);
    assert!(adopted.status.success(), "{}", text(&adopted.stderr));
    let stdout = text(&adopted.stdout);
    assert!(stdout.contains("next daemon start"), "{stdout}");
    assert!(stdout.contains("every module on this machine"), "{stdout}");
    assert_eq!(home.stored(), format!("{ADOPTED}\n"));

    let shown = home.ck(&["machine", "show"]);
    assert!(shown.status.success(), "{}", text(&shown.stderr));
    let shown = text(&shown.stdout);
    assert!(shown.contains(&format!("file:   {ADOPTED}")), "{shown}");
    assert!(shown.contains("daemon: not running"), "{shown}");

    // The next daemon start serves the adopted id.
    let _daemon = home.boot();
    assert_eq!(home.served_machine_id().as_deref(), Some(ADOPTED));
}

#[test]
fn ck_machine_adopt_refuses_a_running_daemon_and_force_never_changes_what_it_serves() {
    let home = Home::new("machine-id-adopt-running");
    let _daemon = home.boot();
    let original = home.served_machine_id().expect("the daemon serves an id");
    let stored = home.stored();

    let refused = home.ck(&["machine", "adopt", ADOPTED]);
    assert!(!refused.status.success());
    assert!(
        text(&refused.stderr).contains("the daemon is running"),
        "{}",
        text(&refused.stderr)
    );
    assert_eq!(home.stored(), stored, "a refused adopt writes nothing");

    let forced = home.ck(&["machine", "adopt", ADOPTED, "--force"]);
    assert!(forced.status.success(), "{}", text(&forced.stderr));
    assert!(
        text(&forced.stdout).contains(&format!("keeps serving {original} until it restarts")),
        "{}",
        text(&forced.stdout)
    );
    assert_eq!(home.stored(), format!("{ADOPTED}\n"));
    assert_eq!(
        home.served_machine_id().as_deref(),
        Some(original.as_str()),
        "the running daemon's id is fixed for its lifetime"
    );

    let shown = home.ck(&["machine", "show"]);
    let shown_text = text(&shown.stdout);
    assert!(
        shown_text.contains(&format!("file:   {ADOPTED}")),
        "{shown_text}"
    );
    assert!(
        shown_text.contains(&format!("daemon: {original}")),
        "{shown_text}"
    );
    assert!(shown_text.contains("they differ"), "{shown_text}");

    let shown_json: Value =
        serde_json::from_slice(&home.ck(&["--json", "machine", "show"]).stdout).unwrap();
    assert_eq!(shown_json["differ"], true);
    assert_eq!(shown_json["file"]["machine_id"], ADOPTED);
    assert_eq!(shown_json["daemon"]["machine_id"], original.as_str());
}
