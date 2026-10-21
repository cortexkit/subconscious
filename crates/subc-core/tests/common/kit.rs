//! The private-daemon kit: the shipped daemon binary started with every
//! per-user location inside a scratch directory, and the `ck` commands that
//! talk to it.
//!
//! Connection-file discovery (`subc_transport::connection_file::
//! discovery_candidates`) tries `$XDG_RUNTIME_DIR/subc-connection.json`, then
//! `$HOME/.local/share/cortexkit/run/subc-connection.json`, then a per-user
//! file in the temp directory. The second rung is how a client finds the
//! operator's daemon, and it ignores `XDG_DATA_HOME`. So a `ck` given private
//! XDG folders but the real HOME, whose private runtime folder has no
//! connection file yet, reaches the operator's daemon. Every `ck` this kit
//! runs therefore gets its own HOME and temp directory as well as its own XDG
//! folders ([`isolate_client`]), and names the kit's connection file with
//! `--subc`. Without `--subc` it finds no daemon at all, so a command that
//! loses the flag fails instead of answering from somewhere else.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use cortexkit_test_support::{dev_command, ScratchDir};
use serde_json::Value;

const BOOT_TIMEOUT: Duration = Duration::from_secs(30);

/// Gives a client command (ck, the ck app, a test client) its own HOME and
/// temp directory under `root`, so neither the home rung nor the temp rung
/// of connection-file discovery can name a file outside the test. It also
/// drops `SUBC_CONNECTION_FILE`, which discovery treats as the only
/// candidate. The XDG folders are left to the caller, because some tests
/// need the client to share the daemon's data home (the machine id lives
/// there).
pub fn isolate_client(command: &mut Command, root: &Path) -> PathBuf {
    let home = root.join("home");
    let temp = root.join("tmp");
    for dir in [&home, &temp] {
        fs::create_dir_all(dir).unwrap();
    }
    command
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("TMPDIR", &temp)
        .env("TEMP", &temp)
        .env("TMP", &temp)
        .env_remove("SUBC_CONNECTION_FILE");
    home
}

/// [`isolate_client`] plus private XDG folders and Windows profile folders
/// of the client's own, distinct from any daemon's, and a closed release
/// index so no command reaches the network.
pub fn isolate_client_fully(command: &mut Command, root: &Path) {
    isolate_client(command, root);
    let runtime = root.join("runtime");
    let data = root.join("data");
    let config = root.join("config");
    for dir in [&runtime, &data, &config] {
        fs::create_dir_all(dir).unwrap();
    }
    command
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("XDG_DATA_HOME", &data)
        .env("XDG_CONFIG_HOME", &config)
        .env("LOCALAPPDATA", &data)
        .env("APPDATA", &config)
        .env("CK_UPDATE_CACHE_PATH", root.join("update-metadata.json"))
        .env("CK_RELEASE_INDEX_URL", "http://127.0.0.1:0/index.json")
        .env_remove("CK_LOG")
        .env_remove("SUBC_MODULE_ID")
        .env_remove("SUBC_LAUNCH_NONCE");
}

/// One private daemon and its scratch tree: `daemon/` holds the daemon's
/// XDG folders and `client/` the folders every `ck` run here uses.
pub struct Kit {
    root: ScratchDir,
    child: Child,
    module_ids: Vec<String>,
}

impl Kit {
    /// Starts the shipped daemon with `modules` (id, entry) as its
    /// `subc.jsonc` modules and waits for its connection file. Each entry's
    /// `env` gains `FAKE_AFT_PID_PATH`, so the kit can stop the module even
    /// when the daemon is killed.
    pub fn boot(name: &str, modules: &[(&str, Value)]) -> Kit {
        let root = ScratchDir::new(name);
        let daemon = root.join("daemon");
        for dir in ["config/cortexkit", "runtime", "data/cortexkit/run"] {
            fs::create_dir_all(daemon.join(dir)).unwrap();
        }
        let mut entries = serde_json::Map::new();
        for (id, entry) in modules {
            let mut entry = entry.clone();
            entry["env"]["FAKE_AFT_PID_PATH"] =
                Value::String(root.join(format!("{id}.pid")).display().to_string());
            entries.insert((*id).to_string(), entry);
        }
        fs::write(
            daemon.join("config/cortexkit/subc.jsonc"),
            serde_json::to_vec(&serde_json::json!({ "version": 1, "modules": entries })).unwrap(),
        )
        .unwrap();
        // The locations are set on the daemon's command only, so this test
        // process and every other test keep their own.
        let child = dev_command(super::ckdev_subc())
            .env("XDG_DATA_HOME", daemon.join("data"))
            .env("XDG_CONFIG_HOME", daemon.join("config"))
            .env("XDG_RUNTIME_DIR", daemon.join("runtime"))
            .env("SUBC_PORT", "0")
            .env("SUBC_CGROUP_PLACEMENT", "disabled")
            .env_remove("CK_LOG")
            .env_remove("SUBC_CONNECTION_FILE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut kit = Kit {
            root,
            child,
            module_ids: modules.iter().map(|(id, _)| (*id).to_string()).collect(),
        };
        let deadline = Instant::now() + BOOT_TIMEOUT;
        loop {
            if let Some(status) = kit.child.try_wait().unwrap() {
                panic!("the kit daemon exited during boot: {status}");
            }
            if kit.connection().exists() && kit.ck(&["--json", "daemon"]).status.success() {
                return kit;
            }
            assert!(Instant::now() < deadline, "the kit daemon did not start");
            thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// The folder every `ck` this kit runs uses as its HOME, temp and XDG
    /// root.
    pub fn client_root(&self) -> PathBuf {
        self.root.join("client")
    }

    pub fn connection(&self) -> PathBuf {
        self.root
            .join("daemon/runtime")
            .join(subc_transport::CONNECTION_FILE_NAME)
    }

    /// The daemon process the kit started.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The shipped `ck`, isolated from every per-user location outside the
    /// kit and given no connection file.
    pub fn ck_command(&self) -> Command {
        let mut command = dev_command(super::ckdev_ck());
        isolate_client_fully(&mut command, &self.client_root());
        command
    }

    /// Runs `ck --subc <the kit's connection file> <args>`.
    pub fn ck(&self, args: &[&str]) -> Output {
        self.ck_command()
            .arg("--subc")
            .arg(self.connection())
            .args(args)
            .output()
            .unwrap()
    }

    /// The pid the answering daemon reports, read from plain `ck daemon`
    /// (`daemon <version> · pid <pid> · …`).
    pub fn answering_pid(&self) -> u32 {
        let output = self.ck(&["daemon"]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "ck daemon failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        stdout
            .split(" · ")
            .find_map(|part| part.strip_prefix("pid "))
            .and_then(|pid| pid.parse().ok())
            .unwrap_or_else(|| panic!("ck daemon printed no pid: {stdout}"))
    }

    /// Fails unless the daemon answering `ck` is the one this kit started.
    /// Called before any command that changes something.
    pub fn assert_answering(&self) {
        assert_eq!(
            self.answering_pid(),
            self.pid(),
            "the daemon answering ck is not the kit's"
        );
    }

    /// Waits until `ck <args>` succeeds and `ok` holds for its JSON stdout.
    pub fn wait_json(&self, what: &str, args: &[&str], ok: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + BOOT_TIMEOUT;
        loop {
            let output = self.ck(args);
            if output.status.success() {
                if let Ok(value) = serde_json::from_slice::<Value>(&output.stdout) {
                    if ok(&value) {
                        return value;
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "{what}: last ck {args:?} printed {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Kit {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        #[cfg(unix)]
        for id in &self.module_ids {
            if let Some(pid) = fs::read_to_string(self.root.join(format!("{id}.pid")))
                .ok()
                .and_then(|pid| pid.trim().parse::<i32>().ok())
                .and_then(rustix::process::Pid::from_raw)
            {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
        }
        #[cfg(not(unix))]
        let _ = &self.module_ids;
    }
}
