#![cfg(windows)]

//! A daemon-only crash must close carried jobs without affecting its siblings.

use cortexkit_test_support::{ckdev_binary, dev_command, ScratchDir};
use serde_json::{json, Value};
use std::{
    fs,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const MODULE: &str = "carried-windows-crash";
const WAIT: Duration = Duration::from_secs(10);

struct OwnedChild(Child);
impl Drop for OwnedChild {
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
    fn if_owned(pid: u32, parent: u32) -> Option<Self> {
        if subc_os::parent_pid(pid) != Some(parent) {
            return None;
        }
        let process = subc_os::Process::open(pid).ok()??;
        let birth = process.observe()?.start_time;
        (subc_os::parent_pid(pid) == Some(parent)).then_some(Self { process, birth })
    }
    fn alive(&self) -> bool {
        !matches!(self.process.wait_for_exit(Duration::ZERO), Ok(true))
            && self
                .process
                .observe()
                .is_some_and(|now| now.start_time == self.birth)
    }
    fn gone(&self) -> bool {
        let deadline = Instant::now() + WAIT;
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
            let _ = self.process.force_stop(self.birth, Duration::from_secs(5));
        }
    }
}

struct Fixture {
    root: ScratchDir,
    daemons: Vec<OwnedChild>,
    members: Vec<OwnedMember>,
}
impl Fixture {
    fn new() -> Self {
        let root = ScratchDir::new("carried-windows-daemon-crash");
        for path in ["config/cortexkit", "data", "runtime"] {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        let config = json!({"version": 1, "modules": { MODULE: {
            "program": env!("CARGO_BIN_EXE_fake-aft-stub"),
            "protocol": "none", "carry_containment": true,
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
    fn record(&self) -> Option<Value> {
        serde_json::from_slice(
            &fs::read(self.root.join("data/cortexkit/run/live-children.json")).ok()?,
        )
        .ok()
    }
    fn identities(&self) -> Option<(u32, u32)> {
        let record = self.record()?;
        let leader = record["children"].as_array()?.iter().find(|entry| {
            entry["module_id"].as_str() == Some(MODULE) && entry["container"].is_object()
        })?["pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())?;
        let holder = fs::read_to_string(self.root.join("holder.pid"))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        Some((leader, holder))
    }
    fn spawn(&mut self) -> usize {
        let log =
            fs::File::create(self.root.join(format!("daemon-{}.log", self.daemons.len()))).unwrap();
        let daemon = dev_command(ckdev_binary(env!("CARGO_BIN_EXE_ck-subc")))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("HOME", self.root.path())
            .env("USERPROFILE", self.root.path())
            .env("APPDATA", self.root.join("config"))
            .env("LOCALAPPDATA", self.root.join("data"))
            .env("SUBC_PORT", "0")
            .env_remove("CK_LOG")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        self.daemons.push(OwnedChild(daemon));
        self.daemons.len() - 1
    }
    fn capture(&mut self, index: usize) -> (u32, u32) {
        let deadline = Instant::now() + WAIT;
        loop {
            assert!(
                self.daemons[index].0.try_wait().unwrap().is_none(),
                "the isolated daemon must stay alive during startup"
            );
            if self.root.join("leader.ready").exists() {
                if let Some((leader, holder)) = self.identities() {
                    let first = OwnedMember::if_owned(leader, self.daemons[index].0.id());
                    let second = OwnedMember::if_owned(holder, leader);
                    if let (Some(first), Some(second)) = (first, second) {
                        self.members.extend([first, second]);
                        return (leader, holder);
                    }
                }
            }
            assert!(
                Instant::now() < deadline,
                "the carried leader/holder must have proven parent links: record {:?}; log {}",
                self.record(),
                fs::read_to_string(self.root.join(format!("daemon-{index}.log")))
                    .unwrap_or_default()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // A failed startup may precede capture(). Only add handles whose parent
        // is our still-owned daemon or its own verified direct child. Already
        // captured handles also cover a failed crash assertion after reparenting.
        if let Some((leader, holder)) = self.identities() {
            for daemon in &self.daemons {
                if let Some(first) = OwnedMember::if_owned(leader, daemon.0.id()) {
                    self.members.push(first);
                    if let Some(second) = OwnedMember::if_owned(holder, leader) {
                        self.members.push(second);
                    }
                    break;
                }
            }
        }
        for daemon in &mut self.daemons {
            let _ = daemon.0.kill();
            let _ = daemon.0.wait();
        }
    }
}

#[test]
fn daemon_crash_closes_carried_windows_job_without_touching_sentinel() {
    let mut fixture = Fixture::new();
    // This is the test's direct child, created before the daemon, not a member
    // of the daemon's root job. Its inherited stdio cannot keep daemon pipes open.
    let mut sentinel = OwnedChild(
        Command::new(env!("CARGO_BIN_EXE_fake-aft-stub"))
            .env("FAKE_AFT_GRANDCHILD_MODE", "1")
            .env_remove("FAKE_AFT_PID_PATH")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert_eq!(
        subc_os::parent_pid(sentinel.0.id()),
        Some(std::process::id())
    );
    let first = fixture.spawn();
    let (old_leader, old_holder) = fixture.capture(first);
    assert!(fixture.members[1].alive());
    // Stop only the retained daemon Child handle: no taskkill /T or descendant
    // traversal is allowed to supply the guarantee this test assigns to jobs.
    fixture.daemons[first].0.kill().unwrap();
    fixture.daemons[first].0.wait().unwrap();
    assert!(
        fixture.members[1].gone(),
        "daemon exit must close the carried job and kill its original holder"
    );
    assert!(
        fixture.members[0].gone(),
        "the leader must die with the daemon too"
    );
    assert!(
        sentinel.0.try_wait().unwrap().is_none(),
        "the independently spawned sentinel must survive every daemon cleanup"
    );
    for path in ["leader.ready", "holder.pid"] {
        fs::remove_file(fixture.root.join(path)).unwrap();
    }
    let second = fixture.spawn();
    let (new_leader, new_holder) = fixture.capture(second);
    assert_ne!(new_leader, old_leader);
    assert_ne!(new_holder, old_holder);
    assert!(fixture.members[3].alive());
    assert!(sentinel.0.try_wait().unwrap().is_none());
}
