#![cfg(windows)]

use cortexkit_test_support::{ckdev_binary, dev_command, ScratchDir};
use serde_json::json;
use std::{
    fs::{self, File},
    io::Write as _,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};
use subc_client_rs::{CallOptions, SubcConsumer};
use subc_protocol::{BindIdentity, RouteTarget};
use tokio::{
    io::AsyncReadExt,
    time::{sleep, timeout, Instant},
};

const MODULE: &str = "force-kill-crash";
const WAIT: Duration = Duration::from_secs(15);

struct Iteration {
    root: ScratchDir,
    daemon: Option<tokio::process::Child>,
    observer: Child,
}

impl Iteration {
    async fn start() -> Self {
        let root = ScratchDir::new("windows-daemon-force-kill");
        for dir in ["config/cortexkit", "data", "runtime"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(
            root.join("config/cortexkit/subc.jsonc"),
            serde_json::to_vec(&json!({
                "version": 1,
                "log": { "level": "debug" },
                "modules": {
                    MODULE: {
                        "program": env!("CARGO_BIN_EXE_fake-aft-stub"),
                        "restart": { "backoff_ms": 100, "max_restarts": 3 },
                        "env": {
                            "FAKE_AFT_MODULE_ID": MODULE,
                            "FAKE_AFT_CRASH_AFTER_MS": "2500",
                            "FAKE_AFT_CONTAINMENT_WITNESS": "1",
                            "FAKE_AFT_EVENTS_PATH": root.join("stub-events.jsonl")
                        }
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let observer_log = File::create(root.join("observer-output.log")).unwrap();
        let observer = Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/daemon-kill-probe.ps1"))
            .arg("-Root")
            .arg(root.path())
            .arg("-HarnessPid")
            .arg(std::process::id().to_string())
            .stdin(Stdio::null())
            .stdout(observer_log.try_clone().unwrap())
            .stderr(observer_log)
            .spawn()
            .unwrap();
        let mut iteration = Self {
            root,
            daemon: None,
            observer,
        };
        iteration.wait_file("observer.ready").await;
        let mut command = dev_command(ckdev_binary(env!("CARGO_BIN_EXE_ck-subc")));
        command
            .env("XDG_CONFIG_HOME", iteration.root.join("config"))
            .env("XDG_DATA_HOME", iteration.root.join("data"))
            .env("XDG_RUNTIME_DIR", iteration.root.join("runtime"))
            .env("HOME", iteration.root.path())
            .env("USERPROFILE", iteration.root.path())
            .env("LOCALAPPDATA", iteration.root.join("data"))
            .env("SUBC_PORT", "0")
            .env_remove("CK_LOG")
            .env_remove("FAKE_AFT_RESTRICT_PROCESS")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        let daemon = command.spawn().unwrap();
        let pid = daemon.id().unwrap();
        iteration.daemon = Some(daemon);
        fs::write(iteration.root.join("daemon.pid.tmp"), pid.to_string()).unwrap();
        fs::rename(
            iteration.root.join("daemon.pid.tmp"),
            iteration.root.join("daemon.pid"),
        )
        .unwrap();
        iteration
    }

    fn connection(&self) -> PathBuf {
        self.root
            .join("runtime")
            .join(subc_transport::CONNECTION_FILE_NAME)
    }

    fn probe(&self) -> String {
        fs::read_to_string(self.root.join("probe.log")).unwrap_or_default()
    }

    fn diagnostics(&self) -> String {
        fn logs(path: &Path, text: &mut String) {
            for entry in fs::read_dir(path).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    logs(&path, text);
                } else if path.extension().is_some_and(|extension| extension == "log")
                    || path
                        .file_name()
                        .is_some_and(|name| name == "stub-events.jsonl")
                {
                    text.push_str(&format!(
                        "\n--- {} ---\n{}",
                        path.display(),
                        fs::read_to_string(&path).unwrap_or_default()
                    ));
                }
            }
        }
        let mut text = String::new();
        logs(self.root.path(), &mut text);
        text
    }

    async fn wait_file(&mut self, name: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.root.join(name).exists() {
            let exited = self.observer.try_wait().unwrap();
            assert!(
                exited.is_none() && Instant::now() < deadline,
                "observer must produce {name}; exited={exited:?}{}",
                self.diagnostics()
            );
            sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_probe(&mut self, predicate: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            let probe = self.probe();
            if predicate(&probe) {
                return probe;
            }
            let exited = self.observer.try_wait().unwrap();
            assert!(
                exited.is_none() && Instant::now() < deadline,
                "observer did not record expected process transition; exited={exited:?}{}",
                self.diagnostics()
            );
            sleep(Duration::from_millis(10)).await;
        }
    }
    async fn witness(&mut self, parent: &Identity) -> Identity {
        let deadline = Instant::now() + WAIT;
        loop {
            let events =
                fs::read_to_string(self.root.join("stub-events.jsonl")).unwrap_or_default();
            for line in events.lines() {
                let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                if event["kind"] == "containment_witness_spawned"
                    && event["parent_pid"].as_u64() == Some(u64::from(parent.pid))
                    && event["parent_creation"]
                        .as_u64()
                        .map(|time| time.to_string())
                        .as_ref()
                        == Some(&parent.creation)
                {
                    return Identity {
                        pid: u32::try_from(event["pid"].as_u64().expect("witness PID")).unwrap(),
                        creation: event["creation"]
                            .as_u64()
                            .expect("witness creation time")
                            .to_string(),
                    };
                }
            }
            assert!(Instant::now() < deadline && !exited(&self.probe(), parent),
                "registered stub must record its parked grandchild before crashing: parent={parent:?}{}", self.diagnostics());
            sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for Iteration {
    fn drop(&mut self) {
        if let Some(daemon) = self.daemon.as_mut() {
            if let Some(pid) = daemon.id() {
                let _ = Command::new("taskkill.exe")
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .stdin(Stdio::null())
                    .output();
            }
            let _ = daemon.start_kill();
        }
        // The PowerShell probe retains native handles to the daemon's descendants.
        // Let it record survivors and terminate those exact process instances
        // before exiting, even when a test assertion fails.
        if self.root.join("daemon.pid").exists() {
            let _ = fs::write(self.root.join("killed"), "killed");
            let _ = self.observer.wait();
        } else {
            let _ = self.observer.kill();
            let _ = self.observer.wait();
        }
    }
}

#[derive(Clone, Debug)]
struct Identity {
    pid: u32,
    creation: String,
}

fn stubs(probe: &str, daemon_pid: u32) -> Vec<Identity> {
    probe
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split('|').collect();
            (fields.len() == 7
                && fields[0] == "seen"
                && fields[3] == daemon_pid.to_string()
                && fields[6].ends_with("fake-aft-stub.exe"))
            .then(|| Identity {
                pid: fields[2].parse().unwrap(),
                creation: fields[4].to_owned(),
            })
        })
        .collect()
}

fn seen(probe: &str, process: &Identity) -> bool {
    probe.lines().any(|line| {
        let fields: Vec<_> = line.split('|').collect();
        fields.len() == 7
            && fields[0] == "seen"
            && fields[2] == process.pid.to_string()
            && fields[4] == process.creation
    })
}

fn in_daemon_job(probe: &str, process: &Identity) -> bool {
    probe.lines().any(|line| {
        let fields: Vec<_> = line.split('|').collect();
        fields.len() == 6
            && fields[0] == "daemon-job"
            && fields[2] == process.pid.to_string()
            && fields[3] == process.creation
            && fields[5] == "member=true"
    })
}

fn exited(probe: &str, process: &Identity) -> bool {
    probe.lines().any(|line| {
        let fields: Vec<_> = line.split('|').collect();
        fields.len() == 4
            && fields[0] == "exit"
            && fields[2] == process.pid.to_string()
            && fields[3] == process.creation
    })
}

async fn drain(
    mut pipe: impl tokio::io::AsyncRead + Unpin,
) -> std::io::Result<(std::time::Instant, Vec<u8>)> {
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes).await?;
    Ok((std::time::Instant::now(), bytes))
}

async fn watch_daemon_exit(
    pid: u32,
) -> (
    u64,
    tokio::sync::oneshot::Receiver<Result<std::time::Instant, String>>,
) {
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let result = (|| {
            let process = subc_os::Process::open(pid)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "daemon gone before handle wait".to_owned())?;
            let observed = process
                .observe()
                .ok_or_else(|| "daemon unobservable".to_owned())?;
            let _ = ready_tx.send(observed.start_time);
            // A dedicated thread waits on the retained process handle before
            // taskkill starts, independent of when taskkill's output is reaped.
            let exited = process
                .wait_for_exit(Duration::from_secs(10))
                .map_err(|error| error.to_string())?;
            let signalled = std::time::Instant::now();
            if exited {
                Ok(signalled)
            } else {
                Err("daemon exit wait timed out".to_owned())
            }
        })();
        let _ = exit_tx.send(result);
    });
    let creation = timeout(WAIT, ready_rx).await.unwrap().unwrap();
    (creation, exit_rx)
}

fn relative_us(event: std::time::Instant, reference: std::time::Instant) -> i128 {
    if event >= reference {
        event.duration_since(reference).as_micros() as i128
    } else {
        -(reference.duration_since(event).as_micros() as i128)
    }
}

fn timing_summary(samples: &mut [i128]) -> String {
    if samples.is_empty() {
        return "n=0,min=NA,median=NA,max=NA".to_owned();
    }
    samples.sort_unstable();
    let middle = samples.len() / 2;
    let median = if samples.len().is_multiple_of(2) {
        (samples[middle - 1] + samples[middle]) / 2
    } else {
        samples[middle]
    };
    format!(
        "n={},min={},median={},max={}",
        samples.len(),
        samples[0],
        median,
        samples[samples.len() - 1]
    )
}

#[test]
fn eof_timing_preserves_events_before_taskkill_return() {
    let before = std::time::Instant::now();
    let after = before + Duration::from_micros(200);
    assert_eq!(relative_us(before, after), -200);
    assert_eq!(relative_us(after, before), 200);
    assert_eq!(relative_us(before, before), 0);
}

#[test]
fn eof_timing_summary_reports_signed_min_median_and_max() {
    assert_eq!(timing_summary(&mut []), "n=0,min=NA,median=NA,max=NA");
    assert_eq!(
        timing_summary(&mut [90, -200, 10, 20]),
        "n=4,min=-200,median=15,max=90"
    );
    assert_eq!(
        timing_summary(&mut [90, -200, 10]),
        "n=3,min=-200,median=10,max=90"
    );
}

/// A crash and respawn followed by a hard daemon kill must reap both generations'
/// parked grandchildren as well as the EOF-cooperative modules and their pipes.
/// Alternating tree and daemon-only kills prevents taskkill from masking the
/// replacement's kill-on-close containment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_crash_respawn_kills_descendants_and_closes_pipes() {
    let iterations = std::env::var("SUBC_WINDOWS_KILL_ITERATIONS")
        .map(|value| value.parse::<usize>().expect("positive iteration count"))
        .unwrap_or(10);
    assert!(
        iterations >= 2,
        "the regression must execute both kill modes"
    );
    let mut attempted = [0_usize; 2];
    let mut in_window = [0_usize; 2];
    let mut survivor_iterations = 0;
    let mut witness_survivor_iterations = [0_usize; 2];
    let mut stdout_missing_eof = 0;
    let mut stderr_missing_eof = 0;
    let mut stdout_from_kill_return = Vec::new();
    let mut stderr_from_kill_return = Vec::new();
    let mut stdout_from_daemon_exit = Vec::new();
    let mut stderr_from_daemon_exit = Vec::new();
    println!("stdio: this harness explicitly gives daemon stdout/stderr Stdio::piped (harness-owned pipes); plain Command::spawn defaults to Stdio::inherit (test stdout/stderr); supervised modules redirect their own stdout/stderr to pipes");
    // EOF and exit timestamps are observation times, not kernel transition times.
    // Negative deltas mean EOF was observed before the reference event. Nextest's
    // leak timeout starts at test-binary exit, not at either reference used here.
    for iteration in 1..=iterations {
        let mode = usize::from(iteration.is_multiple_of(2));
        let mode_name = ["taskkill_tree", "daemon_only"][mode];
        let mut fixture = Iteration::start().await;
        let daemon = fixture.daemon.as_mut().unwrap();
        let pid = daemon.id().unwrap();
        // These are the harness-owned read ends of pipes whose write ends the
        // daemon inherits as its standard output/error, just as under nextest.
        let mut stdout = tokio::spawn(drain(daemon.stdout.take().unwrap()));
        let mut stderr = tokio::spawn(drain(daemon.stderr.take().unwrap()));
        let deadline = Instant::now() + WAIT;
        while subc_transport::read_for_client(fixture.connection()).is_err() {
            assert!(
                Instant::now() < deadline,
                "daemon did not publish its connection{}",
                fixture.diagnostics()
            );
            sleep(Duration::from_millis(10)).await;
        }
        let probe = fixture
            .wait_probe(|probe| !stubs(probe, pid).is_empty())
            .await;
        let first = stubs(&probe, pid).remove(0);
        assert!(
            !exited(&probe, &first),
            "setup must reach the initial process before its first crash{}",
            fixture.diagnostics()
        );
        let first_witness = fixture.witness(&first).await;
        let probe = fixture
            .wait_probe(|probe| seen(probe, &first_witness) || exited(probe, &first))
            .await;
        assert!(
            seen(&probe, &first_witness) && !exited(&probe, &first_witness),
            "initial generation's grandchild must be observed alive: {first_witness:?}{}",
            fixture.diagnostics()
        );
        let client = Arc::new(
            SubcConsumer::connect(&fixture.connection(), Default::default())
                .await
                .unwrap(),
        );
        let call_client = Arc::clone(&client);
        let identity = BindIdentity::new("kill-regression", "crash-window");
        let call = tokio::spawn(async move {
            call_client
                .call(
                    RouteTarget::ToolProvider {
                        module_id: MODULE.to_owned(),
                    },
                    identity,
                    serde_json::to_vec(
                        &json!({ "name": "fake_read", "arguments": { "delay_ms": 30_000 } }),
                    )
                    .unwrap(),
                    CallOptions {
                        timeout: WAIT,
                        ..Default::default()
                    },
                )
                .await
        });
        let events = fixture.root.join("stub-events.jsonl");
        let deadline = Instant::now() + WAIT;
        // The stub records request_received on delivery, but tool_call only after
        // the response delay. Its 2.5s crash interrupts the 30s delay, so only the
        // first event can prove that the request was in flight before the crash.
        while !fs::read_to_string(&events)
            .unwrap_or_default()
            .lines()
            .any(|line| {
                serde_json::from_str::<serde_json::Value>(line).is_ok_and(|event| {
                    event["kind"] == "request_received"
                        && event["body_json"]["name"] == "fake_read"
                        && event["body_json"]["arguments"]["delay_ms"] == 30_000
                })
            })
        {
            assert!(
                !call.is_finished()
                    && !exited(&fixture.probe(), &first)
                    && Instant::now() < deadline,
                "long-running call must reach the stub before its crash{}",
                fixture.diagnostics()
            );
            sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !exited(&fixture.probe(), &first),
            "initial stub must receive the call before crashing"
        );
        fixture.wait_probe(|probe| exited(probe, &first)).await;
        // Schedule the daemon's hard kill 1800ms after the initial stub's
        // observed exit, not after a later restart or client/diagnostic work.
        let crash_observed = Instant::now();
        let probe = fixture
            .wait_probe(|probe| stubs(probe, pid).len() >= 2)
            .await;
        let replacement = stubs(&probe, pid).remove(1);
        assert_ne!(
            first.creation, replacement.creation,
            "a new process instance must be observed"
        );
        assert!(
            timeout(Duration::from_millis(500), call)
                .await
                .unwrap()
                .unwrap()
                .is_err(),
            "the delayed call must be interrupted by the stub crash"
        );
        let replacement_witness = fixture.witness(&replacement).await;
        let probe = fixture
            .wait_probe(|probe| seen(probe, &replacement_witness) || exited(probe, &replacement))
            .await;
        assert!(
            seen(&probe, &replacement_witness) && !exited(&probe, &replacement_witness),
            "replacement's grandchild must be observed alive: {replacement_witness:?}{}",
            fixture.diagnostics()
        );

        // Record which modules belong to the daemon's kill-on-close job before
        // the daemon dies. The probe closes its temporary Job Object handles
        // before acknowledging readiness, or it would itself keep the job alive.
        tokio::time::sleep_until(crash_observed + Duration::from_millis(1200)).await;
        fs::write(fixture.root.join("prepare"), "prepare").unwrap();
        fixture.wait_file("prepared").await;
        let (daemon_creation, daemon_exit) = watch_daemon_exit(pid).await;
        tokio::time::sleep_until(crash_observed + Duration::from_millis(1800)).await;
        let at_kill = fixture.probe();
        assert!(in_daemon_job(&at_kill, &replacement_witness),
            "replacement's parked grandchild must belong to the daemon's current job: {replacement_witness:?}{}", fixture.diagnostics());
        let generation_shape = stubs(&at_kill, pid).len() == 2 && !exited(&at_kill, &replacement);
        let kill_delay_ms = crash_observed.elapsed().as_millis();
        let sampled = (1000..2200).contains(&kill_delay_ms) && generation_shape;
        attempted[mode] += 1;
        if sampled {
            in_window[mode] += 1;
        } else {
            eprintln!("iteration={iteration} mode={mode_name} outside sampling window: kill_after_crash_ms={kill_delay_ms} expected_two_generations={generation_shape}");
        }
        let kill = if mode == 0 {
            Some(
                timeout(
                    Duration::from_secs(5),
                    tokio::process::Command::new("taskkill.exe")
                        .args(["/PID", &pid.to_string(), "/T", "/F"])
                        .stdin(Stdio::null())
                        .output(),
                )
                .await
                .unwrap()
                .unwrap(),
            )
        } else {
            // Unlike /T, stopping only this retained daemon handle leaves the
            // parked replacement descendant entirely to kill-on-job-close.
            let process = subc_os::Process::open(pid)
                .unwrap()
                .expect("daemon alive before direct kill");
            assert!(
                process
                    .force_stop(daemon_creation, Duration::from_secs(5))
                    .unwrap(),
                "daemon-only TerminateProcess must stop the recorded daemon instance"
            );
            None
        };
        let kill_returned = std::time::Instant::now();
        let daemon = fixture.daemon.as_mut().unwrap();
        let _ = daemon.start_kill();
        let status = timeout(Duration::from_secs(5), daemon.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(
            !status.success(),
            "teardown must force-kill, not gracefully drain"
        );
        let daemon_signalled = timeout(Duration::from_secs(5), daemon_exit)
            .await
            .unwrap()
            .unwrap()
            .expect("daemon process handle must signal exit");
        fs::write(fixture.root.join("killed"), "killed").unwrap();

        // Record EOF deadlines before the observer's five-second survivor dump
        // and cleanup, so cleaning up an actual leak cannot turn this green.
        let (stdout_eof, stderr_eof) = tokio::join!(
            timeout(Duration::from_secs(3), &mut stdout),
            timeout(Duration::from_secs(3), &mut stderr),
        );
        fixture.wait_file("probe.done").await;
        let probe = fixture.probe();
        let survivors = fs::read_to_string(fixture.root.join("probe.done")).unwrap();
        let first_witness_exited = exited(&probe, &first_witness);
        let replacement_witness_exited = exited(&probe, &replacement_witness);
        if !first_witness_exited || !replacement_witness_exited {
            witness_survivor_iterations[mode] += 1;
        }
        let stdout_at = match &stdout_eof {
            Ok(Ok(Ok((at, _)))) => Some(*at),
            _ => None,
        };
        let stderr_at = match &stderr_eof {
            Ok(Ok(Ok((at, _)))) => Some(*at),
            _ => None,
        };
        if survivors != "0" {
            survivor_iterations += 1;
        }
        if stdout_at.is_none() {
            stdout_missing_eof += 1;
        }
        if stderr_at.is_none() {
            stderr_missing_eof += 1;
        }
        let stdout_kill_return_us = stdout_at.map(|at| relative_us(at, kill_returned));
        let stderr_kill_return_us = stderr_at.map(|at| relative_us(at, kill_returned));
        let stdout_daemon_us = stdout_at.map(|at| relative_us(at, daemon_signalled));
        let stderr_daemon_us = stderr_at.map(|at| relative_us(at, daemon_signalled));
        if let Some(value) = stdout_kill_return_us {
            stdout_from_kill_return.push(value);
        }
        if let Some(value) = stderr_kill_return_us {
            stderr_from_kill_return.push(value);
        }
        if let Some(value) = stdout_daemon_us {
            stdout_from_daemon_exit.push(value);
        }
        if let Some(value) = stderr_daemon_us {
            stderr_from_daemon_exit.push(value);
        }
        let transitions: Vec<_> = probe
            .lines()
            .filter(|line| !line.starts_with("handle|"))
            .collect();
        let kill_status = kill.as_ref().map_or_else(
            || "TerminateProcess".to_owned(),
            |output| output.status.to_string(),
        );
        println!("iteration={iteration}/{iterations} mode={mode_name} daemon={pid} daemon_creation={daemon_creation} first={first:?} first_grandchild={first_witness:?} first_grandchild_exited={first_witness_exited} replacement={replacement:?} replacement_grandchild={replacement_witness:?} replacement_grandchild_exited={replacement_witness_exited} kill_after_crash_ms={kill_delay_ms} sampled={sampled} kill={kill_status} stdout_eof={} stderr_eof={} stdout_from_kill_return_us={stdout_kill_return_us:?} stderr_from_kill_return_us={stderr_kill_return_us:?} stdout_from_daemon_exit_us={stdout_daemon_us:?} stderr_from_daemon_exit_us={stderr_daemon_us:?}\n{}",
            stdout_at.is_some(), stderr_at.is_some(), transitions.join("\n"));
        let passed = survivors == "0"
            && first_witness_exited
            && replacement_witness_exited
            && stdout_at.is_some()
            && stderr_at.is_some();
        if !passed {
            eprintln!(
                "mode={mode_name} kill stdout={} stderr={}{}",
                kill.as_ref()
                    .map(|output| String::from_utf8_lossy(&output.stdout))
                    .unwrap_or_default(),
                kill.as_ref()
                    .map(|output| String::from_utf8_lossy(&output.stderr))
                    .unwrap_or_default(),
                fixture.diagnostics()
            );
        }
        let observer_status = fixture.observer.wait().unwrap();
        assert!(
            observer_status.success(),
            "observer failed{}",
            fixture.diagnostics()
        );
        // Retain the consumer independently of its failed call until teardown;
        // returning the call's error must not close the harness's connection.
        drop(client);
    }
    let required = attempted.map(|count| (count * 4).div_ceil(5));
    let summary = format!("summary iterations={iterations} taskkill_tree_attempted={} taskkill_tree_in_window={} taskkill_tree_required={} daemon_only_attempted={} daemon_only_in_window={} daemon_only_required={} taskkill_tree_grandchild_survivor_iterations={} daemon_only_grandchild_survivor_iterations={} survivor_iterations={survivor_iterations} stdout_missing_eof={stdout_missing_eof} stderr_missing_eof={stderr_missing_eof} stdout_kill_return_us=({}) stderr_kill_return_us=({}) stdout_daemon_exit_us=({}) stderr_daemon_exit_us=({})",
        attempted[0], in_window[0], required[0], attempted[1], in_window[1], required[1],
        witness_survivor_iterations[0], witness_survivor_iterations[1],
        timing_summary(&mut stdout_from_kill_return), timing_summary(&mut stderr_from_kill_return),
        timing_summary(&mut stdout_from_daemon_exit), timing_summary(&mut stderr_from_daemon_exit));
    println!("{summary}");
    // Cargo captures a passing test's stdout; publish timing evidence separately
    // so hosted CI preserves it without requiring a failing assertion.
    if let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY") {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "{summary}").unwrap();
    }
    assert!(
        witness_survivor_iterations == [0, 0],
        "grandchild: taskkill_tree={} daemon_only={} iterations left a recorded first- or replacement-generation grandchild alive",
        witness_survivor_iterations[0], witness_survivor_iterations[1]
    );
    assert!(
        survivor_iterations == 0,
        "survivor: {survivor_iterations}/{iterations} iterations had a live process after the kill"
    );
    assert!(
        stdout_missing_eof == 0 && stderr_missing_eof == 0,
        "EOF: stdout missing EOF={stdout_missing_eof}, stderr missing EOF={stderr_missing_eof} across {iterations} iterations"
    );
    for (mode, mode_name) in ["taskkill_tree", "daemon_only"].into_iter().enumerate() {
        assert!(
            attempted[mode] > 0 && in_window[mode] >= required[mode],
            "sampling: {} {}/{} kills in window, need {} and a nonempty sample",
            mode_name,
            in_window[mode],
            attempted[mode],
            required[mode]
        );
    }
}
