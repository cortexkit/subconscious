#![cfg(any(target_os = "linux", target_os = "macos"))]

//! Recovery after a daemon crash must verify individual members of its recorded
//! container without touching a separately owned group.

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
    fn spawn_daemon(&mut self) -> usize {
        let daemon = ckdev_binary(env!("CARGO_BIN_EXE_ck-subc"));
        fs::create_dir_all(self.root.join("runtime")).unwrap();
        let child = dev_command(daemon)
            .process_group(0)
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
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
                fs::read_dir(self.root.join("data/cortexkit/run/logs")).into_iter().flatten().filter_map(Result::ok)
                    .map(|entry| fs::read_to_string(entry.path()).unwrap_or_default()).collect::<String>());
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
fn daemon_crash_sweeps_carried_group_without_touching_separate_group() {
    let mut fixture = Fixture::new();
    let mut sentinel = OwnedSentinel::spawn();
    let first_daemon = fixture.spawn_daemon();
    let (first_leader, first_holder) = fixture.capture_generation(first_daemon);
    let record: Value = serde_json::from_slice(&fs::read(fixture.record_path()).unwrap()).unwrap();
    let container = record["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| !entry["container"].is_null())
        .unwrap();
    assert_eq!(
        container["container"]["pgid"].as_u64(),
        Some(u64::from(first_leader)),
        "cgroup placement is disabled, so the record must name the pinned first group"
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
    assert!(
        fixture.members[1].wait_gone(),
        "boot must sweep the previous container's holder, not only its old leader"
    );
    assert!(
        fixture.members[0].wait_gone(),
        "boot must stop the previous leader too"
    );
    assert!(
        sentinel.0.try_wait().unwrap().is_none(),
        "a separately owned group must survive verified container recovery"
    );
    let (new_leader, new_holder) = fixture.capture_generation(replacement_daemon);
    assert_ne!(new_leader, first_leader);
    assert_ne!(new_holder, first_holder);
    assert!(
        fixture.members[3].alive(),
        "fresh starts use a new live holder after recovery"
    );
}
