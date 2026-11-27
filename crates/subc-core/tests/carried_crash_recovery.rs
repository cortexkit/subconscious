#![forbid(unsafe_code)]
#![cfg(any(target_os = "linux", target_os = "macos"))]

//! After a crash, the new daemon can stop the recorded first process of a
//! carried container. An unrecorded survivor is left alone and keeps the
//! module from starting until it exits.

use cortexkit_test_support::{ckdev_binary, dev_command, ScratchDir};
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct OwnedDaemon(Child);
impl OwnedDaemon {
    fn crash(&mut self) {
        // std::Child retains its exit status, so kill after wait is a harmless
        // error rather than signalling a PID reused after the child was reaped.
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }
}
impl Drop for OwnedDaemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct OwnedMember {
    process: subc_os::Process,
    birth: u64,
}
impl OwnedMember {
    fn capture(pid: u32, parent: u32) -> Self {
        Self::if_owned(pid, parent)
            .expect("the fixture must prove this process was spawned by its own child")
    }
    fn if_owned(pid: u32, parent: u32) -> Option<Self> {
        if subc_os::parent_pid(pid) != Some(parent) {
            return None;
        }
        let process = subc_os::Process::open(pid).ok()??;
        let birth = process.observe()?.start_time;
        (subc_os::parent_pid(pid) == Some(parent)).then_some(Self { process, birth })
    }
    fn alive(&self) -> bool {
        self.process
            .observe()
            .is_some_and(|identity| identity.start_time == self.birth)
    }
    fn wait_gone(&self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.alive() {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
        true
    }
    fn stop(&self) {
        assert!(self.alive(), "the fixture's holder must still be alive");
        assert!(self.process.signal(subc_os::Signal::Kill).unwrap());
        assert!(self.wait_gone(), "the fixture's holder must exit");
    }
}
impl Drop for OwnedMember {
    fn drop(&mut self) {
        if self.alive() {
            let _ = self.process.signal(subc_os::Signal::Kill);
        }
    }
}

struct OwnedSentinel(Child);
impl OwnedSentinel {
    fn spawn() -> Self {
        Self(
            Command::new("/bin/sleep")
                .arg("600")
                .process_group(0)
                .spawn()
                .unwrap(),
        )
    }
}
impl Drop for OwnedSentinel {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Fixture {
    root: ScratchDir,
    daemons: Vec<OwnedDaemon>,
    members: Vec<OwnedMember>,
}
impl Fixture {
    fn new() -> Self {
        let root = ScratchDir::new("carried-crash-recovery");
        fs::create_dir_all(root.join("config/cortexkit")).unwrap();
        let config = json!({"version": 1, "modules": {"carried-fixture": {
            "program": env!("CARGO_BIN_EXE_fake-aft-stub"),
            "protocol": "none",
            "carry_containment": true,
            "env": {
                "FAKE_AFT_NEVER_CONNECT": "1",
                "FAKE_AFT_NEVER_CONNECT_READY_PATH": root.join("leader.ready"),
                "FAKE_AFT_GRANDCHILD_PID_FILE": root.join("holder.pid")
            }
        }}});
        fs::write(
            root.join("config/cortexkit/subc.jsonc"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        Self {
            root,
            daemons: Vec::new(),
            members: Vec::new(),
        }
    }
    fn record_path(&self) -> PathBuf {
        self.root.join("data/cortexkit/run/live-children.json")
    }
    fn connection_path(&self) -> PathBuf {
        self.root
            .join("runtime")
            .join(subc_transport::CONNECTION_FILE_NAME)
    }
    fn spawn_daemon(&mut self) -> usize {
        let daemon = ckdev_binary(env!("CARGO_BIN_EXE_ck-subc"));
        fs::create_dir_all(self.root.join("runtime")).unwrap();
        let child = dev_command(daemon)
            .process_group(0)
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("SUBC_CONNECTION_FILE", self.connection_path())
            .env("SUBC_PORT", "0")
            .env("SUBC_CGROUP_PLACEMENT", "disabled")
            .env_remove("CK_LOG")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.daemons.push(OwnedDaemon(child));
        self.daemons.len() - 1
    }
    fn ck(&self, args: &[&str]) -> String {
        let output = dev_command(ckdev_binary(env!("CARGO_BIN_EXE_ck")))
            .arg("--subc")
            .arg(self.connection_path())
            .args(args)
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env_remove("CK_LOG")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "ck {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
    fn daemon_logs(&self) -> String {
        fs::read_dir(self.root.join("data/cortexkit/run/logs"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| fs::read_to_string(entry.path()).unwrap_or_default())
            .collect()
    }
    fn wait_for_retained_holder(&mut self, daemon: usize, pgid: u32, holder: u32) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        let pgid = pgid.to_string();
        let holder_pid = format!("pid {holder} (");
        loop {
            assert!(
                self.daemons[daemon].0.try_wait().unwrap().is_none(),
                "replacement daemon must survive startup"
            );
            let record = fs::read_to_string(self.record_path()).unwrap();
            if let Ok(record) = serde_json::from_str::<Value>(&record) {
                if let Some(reason) = record["retained_reasons"][&pgid]["container_unverifiable"]
                    .as_str()
                    .filter(|reason| reason.contains(&holder_pid))
                    .filter(|_| {
                        subc_transport::read_for_client(self.connection_path())
                            .is_ok_and(|info| info.pid == self.daemons[daemon].0.id())
                    })
                {
                    return reason.to_owned();
                }
            }
            assert!(
                Instant::now() < deadline,
                "boot must durably retain the previous container and name pid {holder}: {record}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn assert_held_report(&self, retained: &str, holder: u32) {
        let detail = format!("waiting: container survivors could not be verified: {retained}");
        assert!(detail.contains(&format!("pid {holder} (")), "{detail}");

        let list = self.ck(&["module", "list"]);
        assert!(
            list.lines()
                .any(|line| line.contains("carried-fixture") && line.contains(&detail)),
            "{list}"
        );

        let response = self.ck(&["--json", "provenance", "carried-fixture"]);
        let provenance: Value = serde_json::from_str(&response).unwrap();
        let module = &provenance["modules"][0];
        assert_eq!(module["module_id"], "carried-fixture");
        assert_eq!(module["waiting_for_previous_detail"], detail);
        assert!(module["daemon_observed"]["pid"].is_null(), "{response}");

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let logs = self.daemon_logs();
            if logs.lines().any(|line| {
                line.contains("carried-fixture")
                    && line.contains("blocking fresh starts")
                    && line.contains(retained)
            }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the daemon log must name the held module and pid {holder}: {logs}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn capture_generation(&mut self, daemon: usize) -> (u32, u32) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                self.daemons[daemon].0.try_wait().unwrap().is_none(),
                "daemon must survive startup"
            );
            let pid = |file| {
                fs::read_to_string(self.root.join(file))
                    .ok()?
                    .trim()
                    .parse::<u32>()
                    .ok()
            };
            let record = fs::read(self.record_path())
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
            // The stub's grandchild inherits startup environment and would
            // overwrite a shared PID marker. The durable container names the
            // actual first leader; verify its parent link independently below.
            let leader = record
                .as_ref()
                .and_then(|record| record["children"].as_array())
                .and_then(|children| {
                    children
                        .iter()
                        .find_map(|entry| entry["container"]["pgid"].as_u64())
                })
                .and_then(|pid| u32::try_from(pid).ok());
            if let (Some(leader), Some(holder)) = (leader, pid("holder.pid")) {
                if self.root.join("leader.ready").exists()
                    && record.as_ref().is_some_and(|record| {
                        record["children"].as_array().is_some_and(|children| {
                            children.iter().any(|entry| {
                                entry["pid"].as_u64() == Some(u64::from(leader))
                                    && !entry["container"].is_null()
                            })
                        })
                    })
                {
                    self.members
                        .push(OwnedMember::capture(leader, self.daemons[daemon].0.id()));
                    self.members.push(OwnedMember::capture(holder, leader));
                    return (leader, holder);
                }
            }
            assert!(Instant::now() < deadline,
                "daemon must publish its carried leader and holder: leader {:?}, holder {:?}, record {:?}, log {}",
                leader, pid("holder.pid"), fs::read_to_string(self.record_path()),
                 self.daemon_logs());
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // A startup assertion may fail before capture_generation records the
        // children. Recover only descendants of a still-owned daemon, proving
        // both parent links and store the kernel start time; cleanup signals a
        // process only if its current start time still matches that record.
        let record = fs::read(self.record_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        for daemon in &self.daemons {
            if subc_os::child_exit::observe(daemon.0.id()).is_err() {
                continue;
            }
            for entry in record
                .as_ref()
                .and_then(|record| record["children"].as_array())
                .into_iter()
                .flatten()
            {
                let Some(pid) = entry["pid"]
                    .as_u64()
                    .and_then(|pid| u32::try_from(pid).ok())
                else {
                    continue;
                };
                if let Some(leader) = OwnedMember::if_owned(pid, daemon.0.id()) {
                    self.members.push(leader);
                    let holder = fs::read_to_string(self.root.join("holder.pid"))
                        .ok()
                        .and_then(|text| text.trim().parse().ok());
                    if let Some(holder) =
                        holder.and_then(|holder| OwnedMember::if_owned(holder, pid))
                    {
                        self.members.push(holder);
                    }
                }
            }
        }
        // Stop parents before their recorded descendants. All cleanup is by a
        // Child handle or a process handle checked against its recorded kernel
        // start time, never a bare group ID.
        self.daemons.clear();
        self.members.clear();
    }
}

#[test]
fn daemon_crash_holds_carried_module_until_the_unrecorded_holder_exits() {
    let mut fixture = Fixture::new();
    let mut sentinel = OwnedSentinel::spawn();
    let first_daemon = fixture.spawn_daemon();
    let (first_leader, first_holder) = fixture.capture_generation(first_daemon);
    let record: Value = serde_json::from_slice(&fs::read(fixture.record_path()).unwrap()).unwrap();
    let children = record["children"].as_array().unwrap();
    let container = children
        .iter()
        .find(|entry| !entry["container"].is_null())
        .unwrap();
    assert_eq!(
        container["container"]["pgid"].as_u64(),
        Some(u64::from(first_leader)),
        "cgroup placement is disabled, so the record must name the pinned first group"
    );
    assert!(
        children.iter().all(|entry| entry["pid"] != first_holder),
        "the holder must have no independently recorded identity"
    );
    assert_eq!(
        subc_os::process_group::pgid(first_holder).unwrap(),
        Some(first_leader)
    );
    assert_ne!(
        subc_os::process_group::pgid(sentinel.0.id()).unwrap(),
        Some(first_leader)
    );

    fixture.daemons[first_daemon].crash();
    assert!(
        fixture.members[1].alive(),
        "a carried holder outlives the daemon crash until boot recovery"
    );
    for file in ["leader.ready", "holder.pid"] {
        fs::remove_file(fixture.root.join(file)).unwrap();
    }
    let replacement_daemon = fixture.spawn_daemon();
    let retained = fixture.wait_for_retained_holder(replacement_daemon, first_leader, first_holder);
    assert!(
        fixture.members[1].alive(),
        "a cold daemon must not signal an unrecorded holder"
    );
    assert!(
        fixture.members[0].wait_gone(),
        "boot must stop the previous leader too"
    );
    fixture.assert_held_report(&retained, first_holder);
    assert!(fixture.members[1].alive());
    for file in ["leader.ready", "holder.pid"] {
        assert!(
            !fixture.root.join(file).exists(),
            "a new instance started while the old holder was alive: {file}"
        );
    }
    fixture.members[1].stop();
    let (new_leader, new_holder) = fixture.capture_generation(replacement_daemon);
    assert!(
        sentinel.0.try_wait().unwrap().is_none(),
        "a separately owned group must survive verified container recovery"
    );
    assert_ne!(new_leader, first_leader);
    assert_ne!(new_holder, first_holder);
    assert!(
        fixture.members[3].alive(),
        "fresh starts use a new live holder after recovery"
    );
}
