#![cfg(windows)]

use cortexkit_test_support::{ckdev_binary, dev_command, ScratchDir};
use serde_json::{json, Value};

mod common;
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const MODULE: &str = "restricted-process";
const CONTROL: &str = "unrestricted-process";
const WAIT: Duration = Duration::from_secs(60);

fn module_config(module_id: &str, restricted: bool) -> Value {
    let mut env = json!({
        "FAKE_AFT_MODULE_ID": module_id,
        // A health cadence alone does not turn probing on: the daemon only sends
        // health RPCs to a module whose registration (HELLO) lists
        // `health.check` among its control operations, so the stub lists it.
        "FAKE_AFT_ADVERTISE_HEALTH": "1"
    });
    if restricted {
        env["FAKE_AFT_RESTRICT_PROCESS"] = json!("1");
    }
    json!({
        "program": env!("CARGO_BIN_EXE_fake-aft-stub"),
        "health": { "cadence_ms": 100, "on_failing": "report" },
        "env": env
    })
}

struct Daemon {
    root: ScratchDir,
    launcher: Option<Child>,
    process: Option<subc_os::Process>,
}

impl Daemon {
    fn boot() -> Self {
        let root = ScratchDir::new("windows-restricted-process");
        for dir in ["config/cortexkit", "data", "runtime"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(
            root.join("config/cortexkit/subc.jsonc"),
            serde_json::to_vec(&json!({
                "version": 1,
                "modules": {
                    MODULE: module_config(MODULE, true),
                    CONTROL: module_config(CONTROL, false)
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let mut daemon = Self {
            root,
            launcher: None,
            process: None,
        };
        daemon.start();
        daemon
    }

    fn isolate(&self, command: &mut Command) {
        command
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("HOME", self.root.path())
            .env("USERPROFILE", self.root.path())
            .env("LOCALAPPDATA", self.root.join("data"))
            .env("SUBC_PORT", "0")
            .env_remove("FAKE_AFT_RESTRICT_PROCESS")
            .env_remove("CK_LOG");
    }

    fn start(&mut self) {
        assert!(
            self.launcher.is_none(),
            "launcher.process must be reaped before starting another daemon"
        );
        let record = self.root.join("launcher.txt");
        let _ = fs::remove_file(&record);
        let log = File::create(self.root.join("daemon.log")).unwrap();
        let mut command = helper();
        self.isolate(&mut command);
        command
            .arg("-Daemon")
            .arg(ckdev_binary(env!("CARGO_BIN_EXE_ck-subc")))
            .arg("-LaunchRecord")
            .arg(&record)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log));
        self.launcher = Some(command.spawn().unwrap());
        let deadline = Instant::now() + WAIT;
        loop {
            // The launcher writes `<pid>;SeDebugPrivilege=absent` in place, so a
            // read can see it empty or half-written; wait for the whole line. The
            // helper throws instead of writing when the privilege survives, which
            // the launcher-exit check below reports.
            if let Some(pid) = fs::read_to_string(&record).ok().and_then(|record| {
                let (pid, privileges) = record.trim_end().split_once(';')?;
                (privileges == "SeDebugPrivilege=absent").then(|| pid.to_owned())
            }) {
                let pid = pid.parse().unwrap();
                self.process = Some(
                    subc_os::Process::open(pid)
                        .unwrap()
                        .expect("real daemon alive"),
                );
                break;
            }
            let exited = self.launcher.as_mut().unwrap().try_wait().unwrap();
            assert!(
                exited.is_none() && Instant::now() < deadline,
                "launcher.exit_status expected running; observed {exited:?}; launcher.record must be written before {WAIT:?}\n{}",
                self.log()
            );
            thread::sleep(Duration::from_millis(50));
        }
        loop {
            let read = subc_transport::read_for_client(self.connection());
            if let Ok(connection) = &read {
                assert_eq!(
                    connection.pid,
                    self.process.as_ref().unwrap().pid(),
                    "connection.pid must identify the launched daemon"
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "daemon not ready: reading {} gave {:?}\n{}",
                self.connection().display(),
                read.err(),
                self.log()
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn connection(&self) -> PathBuf {
        // The daemon publishes into $XDG_RUNTIME_DIR, which `isolate` points here.
        self.root
            .join("runtime")
            .join(subc_transport::CONNECTION_FILE_NAME)
    }

    fn log(&self) -> String {
        fs::read_to_string(self.root.join("daemon.log")).unwrap_or_default()
    }

    fn ck(&self, args: &[&str]) -> Value {
        let mut command = dev_command(ckdev_binary(env!("CARGO_BIN_EXE_ck")));
        self.isolate(&mut command);
        common::kit::isolate_client(&mut command, &self.root.join("client"));
        let output = command
            .arg("--subc")
            .arg(self.connection())
            .arg("--json")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "ck.exit_status for {args:?} must succeed; observed {}\n{}\ndaemon: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
            self.log()
        );
        serde_json::from_slice(&output.stdout).expect("ck JSON response")
    }

    fn wait_status(
        &self,
        module_id: &str,
        expected: &[(&str, Value)],
        require_probe: bool,
    ) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            let status = self.ck(&["module", "status", module_id]);
            let mut unsettled: Vec<_> = expected
                .iter()
                .filter_map(|(field, expected)| {
                    let observed = status.pointer(field).unwrap_or(&Value::Null);
                    (observed != expected)
                        .then(|| format!("{field}: expected {expected}, observed {observed}"))
                })
                .collect();
            if require_probe
                && !status["module"]["last_probe_ms"]
                    .as_u64()
                    .is_some_and(|time| time > 0)
            {
                unsettled.push(format!(
                    "/module/last_probe_ms: expected completed-probe timestamp, observed {}",
                    status["module"]["last_probe_ms"]
                ));
            }
            if unsettled.is_empty() {
                return status;
            }
            if Instant::now() >= deadline {
                let control_status =
                    (module_id == MODULE).then(|| self.ck(&["module", "status", CONTROL]));
                panic!("{module_id}: fields did not settle: {}\nstatus: {status}\nunrestricted control: {control_status:?}\ndaemon: {}", unsettled.join("; "), self.log());
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn healthy_pid(&self, module_id: &str) -> u32 {
        self.wait_status(
            module_id,
            &[
                ("/module/state", json!("running")),
                ("/module/live", json!(true)),
                ("/health/status", json!("ok")),
            ],
            true,
        );
        let provenance = self.ck(&["provenance", module_id]);
        let observed = &provenance["modules"][0]["daemon_observed"];
        assert_eq!(
            observed["running_image"]["status"], "match",
            "{module_id}: daemon_observed.running_image.status: {provenance}"
        );
        assert_eq!(
            PathBuf::from(observed["spawned_from"].as_str().unwrap()),
            fs::canonicalize(env!("CARGO_BIN_EXE_fake-aft-stub")).unwrap(),
            "{module_id}: daemon_observed.spawned_from must identify fake-aft-stub"
        );
        u32::try_from(
            observed["pid"]
                .as_u64()
                .expect("daemon_observed.pid must be present"),
        )
        .expect("daemon_observed.pid must fit u32")
    }

    fn healthy_process(&self) -> ModuleProcess {
        let pid = self.healthy_pid(MODULE);
        let start_time = assert_memory_read_denied(pid);
        ModuleProcess { pid, start_time }
    }

    fn kill_hard(&mut self) {
        let process = self.process.take().unwrap();
        let observed = process.observe().expect("daemon alive before hard kill");
        assert!(
            process.force_stop(observed.start_time, WAIT).unwrap(),
            "daemon.process_exit did not complete after hard kill"
        );
        let status = self.launcher.take().unwrap().wait().unwrap();
        assert!(
            !status.success(),
            "daemon.exit_status must indicate forced termination, not a graceful drain"
        );
        fs::remove_file(self.connection()).unwrap();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(process) = self.process.take() {
            if let Some(observed) = process.observe() {
                let _ = process.force_stop(observed.start_time, WAIT);
            }
        }
        if let Some(mut launcher) = self.launcher.take() {
            let _ = launcher.kill();
            let _ = launcher.wait();
        }
    }
}

fn helper() -> Command {
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/process-acl-helper.ps1"));
    command
}

fn assert_memory_read_denied(pid: u32) -> u64 {
    let output = helper()
        .arg("-TargetPid")
        .arg(pid.to_string())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "opener.exit_status must succeed after stripping SeDebugPrivilege; observed {}\n{stdout}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("SeDebugPrivilege=absent"),
        "opener.SeDebugPrivilege must be absent: {stdout}"
    );
    assert!(
        stdout.contains("same-user; narrow-open=ok; PROCESS_VM_READ=ERROR_ACCESS_DENIED(5)"),
        "OpenProcess(PROCESS_VM_READ).error must be ERROR_ACCESS_DENIED(5), with same-user narrow open succeeding: {stdout}"
    );
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("creation-time="))
        .expect("opener must observe creation time")
        .parse()
        .unwrap()
}

struct ModuleProcess {
    pid: u32,
    start_time: u64,
}

impl ModuleProcess {
    fn assert_exited(&self, context: &str) {
        let output = helper()
            .arg("-WaitGone")
            .arg("-TargetPid")
            .arg(self.pid.to_string())
            .arg("-TargetStartTime")
            .arg(self.start_time.to_string())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{context}: process.exit_status for pid {} must confirm termination; helper status: {}\n{}",
            self.pid,
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("exit=confirmed"),
            "process.exit_status for pid {}: helper omitted exit=confirmed",
            self.pid
        );
    }
}

/// The Windows supervisor observes modules through a process handle obtained at
/// spawn, before the module replaces its DACL. Killing the daemon closes its
/// KILL_ON_JOB_CLOSE job handles, so its supervised modules die before relaunch.
/// To force the replacement daemon to open a live process AFTER self-restriction,
/// this test also starts a stub outside the daemon's job and records its creation
/// time in live-children.json. The replacement daemon must terminate that stub
/// through its startup orphan sweep, not through an inherited spawn handle.
#[test]
fn restricted_module_registers_restarts_stops_and_windows_orphan_sweep_works() {
    let mut daemon = Daemon::boot();
    // The same daemon must probe the unrestricted fixture before the restricted
    // one is judged: a stalled control points to configuration, not a DACL denial.
    daemon.healthy_pid(CONTROL);
    let first = daemon.healthy_process();

    daemon.ck(&["module", "restart", MODULE]);
    let second = daemon.healthy_process();
    assert_ne!(
        first.pid, second.pid,
        "module.pid did not change after restart"
    );
    first.assert_exited("old module survived restart");

    daemon.ck(&["module", "stop", MODULE]);
    daemon.wait_status(
        MODULE,
        &[
            // `ck module stop` disables the module, so its state reads disabled.
            ("/module/state", json!("disabled")),
            ("/module/enabled", json!(false)),
            ("/module/live", json!(false)),
        ],
        false,
    );
    second.assert_exited("stopped module remains alive");

    daemon.ck(&["module", "start", MODULE]);
    let before_crash = daemon.healthy_process();
    daemon.kill_hard();
    // Windows containment uses KILL_ON_JOB_CLOSE. Hard daemon death closes the
    // last job handle, so the module must be dead BEFORE a new orphan sweep runs.
    before_crash.assert_exited("restricted child outlived the daemon job");
    let orphan = RecordedOrphan::seed(&daemon);
    daemon.start();
    orphan.assert_swept(&daemon);
    daemon.healthy_pid(CONTROL);
    let after_crash = daemon.healthy_process();
    assert_ne!(
        before_crash.pid, after_crash.pid,
        "module.pid did not change after daemon relaunch"
    );
}

struct RecordedOrphan {
    child: Child,
    image: subc_os::SpawnedImage,
}

impl RecordedOrphan {
    fn seed(daemon: &Daemon) -> Self {
        let program = Path::new(env!("CARGO_BIN_EXE_fake-aft-stub"));
        let capture = subc_os::ExecutableCapture::open(program).unwrap();
        let ready = daemon.root.join("orphan.ready");
        let mut command = Command::new(program);
        daemon.isolate(&mut command);
        let child = command
            .env("FAKE_AFT_RESTRICT_PROCESS", "1")
            .env("FAKE_AFT_NEVER_CONNECT", "1")
            .env("FAKE_AFT_NEVER_CONNECT_READY_PATH", &ready)
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                File::create(daemon.root.join("orphan.log")).unwrap(),
            ))
            .spawn()
            .unwrap();
        // Keep the original spawn handle instead of opening the child by PID.
        // If the daemon's PID-open implementation adds PROCESS_VM_READ, setup
        // must still succeed so the orphan-sweep assertion detects the regression.
        let image = capture.bind(&child).unwrap();
        let mut orphan = Self { child, image };
        let deadline = Instant::now() + WAIT;
        while !ready.exists() {
            let exited = orphan.child.try_wait().unwrap();
            assert!(
                exited.is_none() && Instant::now() < deadline,
                "recorded_orphan.ready_marker missing; recorded_orphan.exit_status expected running, observed {exited:?}"
            );
            thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            assert_memory_read_denied(orphan.child.id()),
            orphan.image.start_time(),
            "recorded_orphan.start_time must match the narrow opener's creation-time observation"
        );
        assert!(
            matches!(
                orphan.image.agreement(orphan.image.start_time()),
                subc_os::ImageAgreement::Match(_)
            ),
            "recorded_orphan.running_image must match the spawned fixture"
        );
        // The Windows startup sweep checks the recorded creation time on the
        // same handle it uses to terminate the process. It does not use Unix
        // device/inode image identity, so executable is null in this record.
        let path = daemon.root.join("data/cortexkit/run/live-children.json");
        fs::write(
            path,
            serde_json::to_vec(&json!({
                "version": 1,
                "children": [{
                    "module_id": "restricted-recorded-orphan",
                    "pid": orphan.child.id(),
                    "protocol": "none",
                    "start_time": orphan.image.start_time(),
                    "executable": null,
                    "cgroup_name": null
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        orphan
    }

    fn assert_swept(&self, daemon: &Daemon) {
        assert!(self.image.process().wait_for_exit(WAIT).unwrap(),
            "seeded_restricted_orphan_is_swept: recorded_orphan.process_exit did not complete; replacement daemon could not open and stop the restricted recorded child\n{}", daemon.log());
        assert!(
            self.image.process().observe().is_none(),
            "recorded_orphan.observation must be absent after the startup sweep"
        );
    }
}

impl Drop for RecordedOrphan {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
