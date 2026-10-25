#![cfg(unix)]

use cortexkit_test_support::{dev_command, process_alive, ScratchDir};
use serde_json::json;
use std::{
    fs,
    os::unix::process::CommandExt,
    process::{Child, Stdio},
    thread,
    time::{Duration, Instant},
};

mod common;

struct Daemon {
    child: Child,
    root: ScratchDir,
}

impl Daemon {
    fn pids(&self) -> Vec<i32> {
        fs::read_to_string(self.root.join("pids"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.parse().ok())
            .collect()
    }

    fn log(&self) -> String {
        fs::read_dir(self.root.join("data/cortexkit/run/logs"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("subc"))
            .map(|entry| fs::read_to_string(entry.path()).unwrap_or_default())
            .collect()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            // Only the daemon still owns unreaped handles for its modules.
            // Let shutdown stop them; pids in the fixture file may be reused.
            if let Some(pid) = i32::try_from(self.child.id())
                .ok()
                .filter(|pid| *pid > 1)
                .and_then(rustix::process::Pid::from_raw)
            {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
            }
            let deadline = Instant::now() + Duration::from_secs(35);
            while Instant::now() < deadline {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn real_daemon_registration_timeout_kills_respawns_and_warns() {
    let root = ScratchDir::new("registration-timeout");
    for dir in ["config/cortexkit", "runtime", "data"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    let script = root.join("never-hello.sh");
    // Exit zero on TERM to prove timeout classification does not depend on the
    // process's signal disposition. No wire connection or HELLO is attempted.
    fs::write(
        &script,
        "trap 'exit 0' TERM\necho $$ >> \"$PID_FILE\"\nwhile :; do sleep 1; done\n",
    )
    .unwrap();
    fs::write(
        root.join("config/cortexkit/subc.jsonc"),
        serde_json::to_vec(&json!({
            "version": 1,
            "modules": { "never-hello": {
                "program": "/bin/sh", "args": [script], "register_timeout_s": 10,
                "drain_timeout_ms": 0,
                "env": {
                    "PID_FILE": root.join("pids"),
                    "XDG_DATA_HOME": root.join("data"),
                    "XDG_CONFIG_HOME": root.join("config"),
                    "XDG_RUNTIME_DIR": root.join("runtime")
                }
            }}
        }))
        .unwrap(),
    )
    .unwrap();
    let child = dev_command(common::ckdev_subc())
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
        .unwrap();
    let mut daemon = Daemon { child, root };
    let deadline = Instant::now() + Duration::from_secs(40);
    let pids = loop {
        assert!(
            daemon.child.try_wait().unwrap().is_none(),
            "daemon exited: {}",
            daemon.log()
        );
        let pids = daemon.pids();
        if pids.len() >= 2 {
            break pids;
        }
        assert!(
            Instant::now() < deadline,
            "unregistered child did not respawn: {}",
            daemon.log()
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert_ne!(pids[0], pids[1]);
    assert!(!process_alive(pids[0]), "timed-out child survived");
    assert!(process_alive(pids[1]), "replacement child is not alive");
    let log = daemon.log();
    let warning = log
        .lines()
        .find(|line| line.contains("first HELLO deadline expired"))
        .unwrap_or_else(|| panic!("missing registration WARN: {log}"));
    assert!(warning.contains("WARN"), "{warning}");
    for field in [
        "never-hello".to_string(),
        pids[0].to_string(),
        "wait_secs=10".to_string(),
    ] {
        assert!(warning.contains(&field), "missing {field}: {warning}");
    }
    let journal =
        fs::read_to_string(daemon.root.join("data/cortexkit/run/terminals.jsonl")).unwrap();
    assert!(journal.contains("registration_timeout"), "{journal}");
    assert!(
        journal.contains("restarting"),
        "timeout was not treated as a crash: {journal}"
    );
    for json_output in [true, false] {
        let mut command = dev_command(common::ckdev_ck());
        common::kit::isolate_client(&mut command, &daemon.root.join("client"));
        command
            .arg("--subc")
            .arg(
                daemon
                    .root
                    .join("runtime")
                    .join(subc_transport::CONNECTION_FILE_NAME),
            )
            .args(["module", "status", "never-hello"]);
        if json_output {
            command.arg("--json");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if json_output {
            let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(status["last_exit_reason"], "registration_timeout");
        } else {
            assert!(String::from_utf8_lossy(&output.stdout)
                .contains("last exit reason: registration_timeout"));
        }
    }
}
