use super::*;

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use cortexkit_test_support::ScratchDir;
use serde_json::{json, Value};
use subc_client_rs::SupervisorListResponse;
use subc_control::{
    DaemonResources, StderrCaptureState, StderrTail, StderrTailEntry, SupervisorEntry,
    SupervisorHealthEntry, SupervisorHealthStatus as Health, TerminalDisposition, TerminalEntry,
    TerminalExitKind, TerminalHistory,
};
use subc_os::{host::DiskSpace, MemoryKind, ResourceUsage};

use crate::{
    stack::{manager::Job, ManagerState, MarkerState},
    window::{
        journal::{self, Action, Liveness, Locked, Owner, Record, Step},
        RevertOutcome, RevertResult,
    },
};

use super::{
    host::{read_windows, ConnectionFence, DoctorDaemon, RpcError, RpcResult},
    module_checks,
    run::{now_ms, run_with, Budget, Limits, Pass},
    stack_checks::{cpu_delta, resources, Binary},
    window_checks::WindowRow,
};

#[cfg(unix)]
mod programs;

struct Fixture {
    scratch: ScratchDir,
    daemon: Arc<FakeDaemon>,
    answers: bool,
    describe_gate: Option<Arc<Gate>>,
    connects: AtomicUsize,
    #[cfg(unix)]
    replacements: Mutex<std::collections::VecDeque<RpcResult<Arc<FakeDaemon>>>>,
    manager: (Result<Job, String>, MarkerState),
    manager_gate: Option<Arc<Gate>>,
    process: Option<ProcessEntry>,
    start_reads: AtomicUsize,
    process_changed: bool,
    epoch_step: bool,
    sampled_cpu: std::sync::atomic::AtomicBool,
    cpu_gate: Option<Arc<Gate>>,
    install_gate: Option<Arc<Gate>>,
    disk_gate: Option<Arc<Gate>>,
    binaries: Result<[bool; 2], String>,
    legacy_ck: bool,
    binary_checks: Mutex<Vec<PathBuf>>,
    versions: BTreeMap<String, String>,
    data: PathBuf,
    free_space: Option<u64>,
    disk_path: Mutex<Option<PathBuf>>,
    windows: Mutex<Vec<WindowRow>>,
    window_gate: Option<Arc<Gate>>,
    configured: Result<Vec<String>, String>,
    outcome: RevertOutcome,
    reverts: Mutex<Vec<String>>,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let scratch = ScratchDir::new(name);
        let data = scratch.path().join("data");
        fs::create_dir(&data).unwrap();
        Self {
            scratch,
            daemon: Arc::new(FakeDaemon::default()),
            answers: true,
            describe_gate: None,
            connects: AtomicUsize::new(0),
            #[cfg(unix)]
            replacements: Mutex::default(),
            manager: (
                Ok(Job {
                    state: ManagerState::Running,
                    pid: None,
                }),
                MarkerState::Absent,
            ),
            manager_gate: None,
            process: None,
            start_reads: AtomicUsize::new(0),
            process_changed: false,
            epoch_step: false,
            sampled_cpu: std::sync::atomic::AtomicBool::new(false),
            cpu_gate: None,
            install_gate: None,
            disk_gate: None,
            binaries: Ok([true, true]),
            legacy_ck: false,
            binary_checks: Mutex::new(Vec::new()),
            versions: BTreeMap::new(),
            data,
            free_space: Some(5 << 30),
            disk_path: Mutex::new(None),
            windows: Mutex::new(Vec::new()),
            window_gate: None,
            configured: Ok(Vec::new()),
            outcome: RevertOutcome::RefusedBeforeChange,
            reverts: Mutex::new(Vec::new()),
        }
    }

    fn set_daemon(&mut self, daemon: FakeDaemon) {
        self.daemon = Arc::new(daemon);
    }

    fn set_manager(&mut self, state: ManagerState, marker: MarkerState, pid: Option<u32>) {
        self.manager = (Ok(Job { state, pid }), marker);
    }

    fn add_window(&self, age_ms: u64, nonce: u16, phase: WindowPhase) -> WindowRow {
        let root = fs::canonicalize(&self.data).unwrap();
        let began = now_ms() - age_ms;
        let id = journal::id::candidate(began, nonce);
        let dir = journal::dir(&root, &id).unwrap();
        fs::create_dir_all(&dir).unwrap();
        journal::create(&dir, &id, began, None).unwrap();
        let mut journal = Locked::open(&root, &id).unwrap();
        let owner = Owner {
            pid: 42,
            start_time: 1,
            executable: Some(root.join("owner")),
            executable_identity: None,
            boot: cfg!(unix).then(|| "01234567-89ab-cdef-0123-0123456789ab".into()),
        };
        let liveness = match phase {
            WindowPhase::Empty => None,
            WindowPhase::Closed => {
                journal.append(Record::Abandoned).unwrap();
                None
            }
            WindowPhase::Revert(liveness) => {
                journal.append(Record::RevertStart { owner }).unwrap();
                Some(liveness)
            }
            WindowPhase::Idle | WindowPhase::Update(_) => {
                journal
                    .append(Record::Step {
                        step: Box::new(Step {
                            target: root.join("provider"),
                            action: Action::File {
                                before: None,
                                after: Some("0".repeat(64)),
                                mode: None,
                            },
                            owner,
                            module: None,
                            role: None,
                            config_generation: None,
                        }),
                    })
                    .unwrap();
                if let WindowPhase::Update(liveness) = phase {
                    Some(liveness)
                } else {
                    journal.append(Record::Done { index: 1 }).unwrap();
                    None
                }
            }
        };
        (Ok(journal.snapshot().clone()), liveness)
    }
}

enum WindowPhase {
    Empty,
    Closed,
    Idle,
    Update(Liveness),
    Revert(Liveness),
}

impl DoctorHost for Fixture {
    fn binaries(&self) -> Result<[Binary; 2], String> {
        if let Some(gate) = &self.install_gate {
            gate.wait();
        }
        self.binaries.as_ref().map_err(Clone::clone)?;
        Ok([
            Binary {
                name: "ck-subc",
                path: self.data.join("ck-subc"),
                signature: None,
                legacy_path: None,
            },
            Binary::ck(&self.data, false),
        ])
    }

    fn binary_present(&self, path: &Path) -> Result<bool, String> {
        self.binary_checks.lock().unwrap().push(path.to_path_buf());
        if path.starts_with(self.data.join("bin")) {
            Ok(self.legacy_ck)
        } else {
            Ok(self.binaries.as_ref().unwrap()
                [usize::from(path.starts_with(self.data.join("cmd")))])
        }
    }

    fn versions(&self) -> Result<BTreeMap<String, String>, String> {
        Ok(self.versions.clone())
    }

    fn manager(&self) -> (Result<Job, String>, MarkerState) {
        if let Some(gate) = &self.manager_gate {
            gate.wait();
        }
        self.manager.clone()
    }

    fn process_entry(&self, _: u32) -> Option<ProcessEntry> {
        self.process.clone()
    }

    fn process_start_time_ms(&self, _: u32) -> Option<u64> {
        Some(42 + 2_000 * u64::from(self.epoch_step && self.sampled_cpu.load(Ordering::Relaxed)))
    }

    fn process_start_time(&self, _: u32) -> Option<u64> {
        let read = self.start_reads.fetch_add(1, Ordering::Relaxed);
        Some(123 + u64::from(self.process_changed && read >= 2))
    }

    fn resource_usage(&self, _: u32) -> Option<ResourceUsage> {
        Some(usage(0, 0, 8))
    }

    fn wait_for_cpu_sample(&self) {
        self.sampled_cpu.store(true, Ordering::Relaxed);
        if let Some(gate) = &self.cpu_gate {
            gate.wait();
        }
    }

    fn data_directory(&self) -> Result<PathBuf, String> {
        Ok(self.data.clone())
    }

    #[cfg(unix)]
    fn settings_directory(&self) -> Option<PathBuf> {
        Some(self.scratch.path().join("settings"))
    }

    #[cfg(unix)]
    fn local_connection_file(&self) -> Result<Option<PathBuf>, String> {
        let path = self.scratch.path().join("run/connection.json");
        Ok(path.exists().then_some(path))
    }

    fn disk_space(&self, path: &Path) -> Option<DiskSpace> {
        if let Some(gate) = &self.disk_gate {
            gate.wait();
        }
        *self.disk_path.lock().unwrap() = Some(path.into());
        self.free_space.map(|free_bytes| DiskSpace {
            free_bytes,
            total_bytes: 20 << 30,
        })
    }

    fn windows(&self) -> Vec<WindowRow> {
        if let Some(gate) = &self.window_gate {
            gate.wait();
        }
        self.windows.lock().unwrap().clone()
    }

    fn configured_modules(&self) -> Result<Vec<String>, String> {
        self.configured.clone()
    }

    fn describe(
        &self,
        _: Option<&Path>,
        _: Instant,
        hint: &dyn Fn(u32),
    ) -> RpcResult<Arc<dyn DoctorDaemon>> {
        let _attempt = self.connects.fetch_add(1, Ordering::Relaxed);
        hint(std::process::id());
        if let Some(gate) = &self.describe_gate {
            gate.wait();
        }
        #[cfg(unix)]
        if _attempt != 0 {
            if let Some(next) = self.replacements.lock().unwrap().pop_front() {
                return next.map(|daemon| daemon as Arc<dyn DoctorDaemon>);
            }
        }
        if self.answers {
            Ok(self.daemon.clone())
        } else {
            Err(RpcError::Failed("no daemon".into()))
        }
    }

    fn revert(&self, id: &str) -> RevertResult {
        self.reverts.lock().unwrap().push(id.into());
        if self.outcome != RevertOutcome::RefusedBeforeChange {
            self.windows.lock().unwrap().clear();
        }
        let mut result = RevertResult::default();
        result.outcome = self.outcome;
        result.report.exit = self.outcome.exit();
        result.report.window = Some(id.into());
        if self.outcome == RevertOutcome::RefusedBeforeChange {
            result.report.message = Some("Approval declined".into());
        }
        result
    }
}

#[derive(Clone, Copy, Default)]
enum Reply {
    #[default]
    Ok,
    Failed,
    TimedOut,
    Lost,
}

impl Reply {
    fn with<T>(self, value: impl FnOnce() -> T) -> RpcResult<T> {
        match self {
            Self::Ok => Ok(value()),
            Self::Failed => Err(RpcError::Failed("read failed".into())),
            Self::TimedOut => Err(RpcError::TimedOut),
            Self::Lost => Err(RpcError::ConnectionLost),
        }
    }
}

#[derive(Default)]
struct FakeDaemon {
    #[cfg(unix)]
    program_script: programs::DaemonScript,
    modules: Vec<SupervisorEntry>,
    resource_row: bool,
    list: Reply,
    list_gate: Option<Arc<Gate>>,
    health: Reply,
    health_data: Vec<SupervisorHealthEntry>,
    terminal: Reply,
    exits: Vec<TerminalEntry>,
    tail: Reply,
    tail_overrides: BTreeMap<String, Reply>,
    tail_data: BTreeMap<String, StderrTail>,
    job_delay: Duration,
    calls: Mutex<Vec<(&'static str, String, Instant)>>,
    caps: Mutex<Vec<(String, u32, u32)>>,
}

impl FakeDaemon {
    fn record(&self, check: &'static str, module: &str) {
        self.calls
            .lock()
            .unwrap()
            .push((check, module.into(), Instant::now()));
    }
}

impl DoctorDaemon for FakeDaemon {
    fn list(&self, _: Instant) -> RpcResult<SupervisorListResponse> {
        self.record("list", "");
        if let Some(gate) = &self.list_gate {
            gate.wait();
        }
        #[cfg(unix)]
        if let Some(result) = self.program_script.next_list() {
            return result.map(|modules| SupervisorListResponse {
                generation: 1,
                modules,
                daemon: None,
                agent_commands: None,
            });
        }
        self.list.with(|| SupervisorListResponse {
            generation: 1,
            modules: self.modules.clone(),
            daemon: self
                .resource_row
                .then(|| DaemonResources::new(123, 42, "fixture")),
            agent_commands: None,
        })
    }

    fn health(&self, _: Instant) -> RpcResult<Vec<SupervisorHealthEntry>> {
        self.record("health", "");
        self.health.with(|| self.health_data.clone())
    }

    fn terminals(&self, module: &str, _: Instant) -> RpcResult<TerminalHistory> {
        self.record("terminals", module);
        thread::sleep(self.job_delay);
        self.terminal.with(|| TerminalHistory {
            daemon_started_at_ms: 0,
            entries: self.exits.clone(),
            dropped: 0,
            journal_skipped_lines: 0,
            journal_read_errors: 0,
            journal_write_failures: 0,
        })
    }

    fn stderr_tail(
        &self,
        module: &str,
        max_lines: u32,
        max_bytes: u32,
        _: Instant,
    ) -> RpcResult<StderrTail> {
        self.record("tail", module);
        self.caps
            .lock()
            .unwrap()
            .push((module.into(), max_lines, max_bytes));
        thread::sleep(self.job_delay);
        self.tail_overrides
            .get(module)
            .copied()
            .unwrap_or(self.tail)
            .with(|| {
                self.tail_data
                    .get(module)
                    .cloned()
                    .unwrap_or_else(|| StderrTail {
                        capture: StderrCaptureState::Captured,
                        entries: Vec::new(),
                        dropped_lines: 0,
                    })
            })
    }

    #[cfg(unix)]
    fn set_enabled(
        &self,
        module: &str,
        enabled: bool,
        deadline: Instant,
    ) -> Result<bool, super::host::SetEnabledError> {
        self.record(if enabled { "start" } else { "stop" }, module);
        self.program_script.set_enabled(module, enabled, deadline)
    }
}

#[derive(Default)]
struct FakeLogs {
    seen: Mutex<Vec<String>>,
    block_at: Option<(mpsc::Sender<()>, Arc<Gate>)>,
}

impl LogFallback for FakeLogs {
    fn tail(&self, module: &str) -> LogFallbackRead {
        self.seen.lock().unwrap().push(module.into());
        if module == "slow" {
            if let Some((entered, gate)) = &self.block_at {
                entered.send(()).unwrap();
                gate.wait();
            }
        }
        LogFallbackRead {
            tail: Ok(Some(LogTail {
                module_id: module.into(),
                entries: (0..20).map(|n| format!("line {n}")).collect(),
                truncated: true,
            })),
            run_folder_warning: Some("run folder could not be resolved".into()),
        }
    }
}

#[derive(Default)]
struct Gate(Mutex<bool>, Condvar);

impl Gate {
    fn wait(&self) {
        drop(
            self.1
                .wait_while(self.0.lock().unwrap(), |open| !*open)
                .unwrap(),
        );
    }

    fn open(&self) {
        *self.0.lock().unwrap() = true;
        self.1.notify_all();
    }
}

const FAKE_WAIT_CAP: Duration = Duration::from_secs(30);
const RPC_TIMEOUT: Duration = Duration::from_millis(120);
const LOCAL_TIMEOUT: Duration = Duration::from_millis(200);

// Only deliberately blocked fixture stages use short deadlines. The cap makes
// a broken fake fail instead of hanging while allowing for delayed scheduling.
fn fixture_limits() -> Limits {
    Limits {
        describe: FAKE_WAIT_CAP,
        rpc: FAKE_WAIT_CAP,
        window: FAKE_WAIT_CAP,
        local: FAKE_WAIT_CAP,
        modules: FAKE_WAIT_CAP,
        ..Limits::default()
    }
}

#[test]
fn delayed_describe_launch_does_not_make_an_answering_fixture_offline() {
    let host = Arc::new(Fixture::new("doctor-delayed-launch"));
    let mut limits = fixture_limits();
    // Hold Describe until a short RPC deadline would have elapsed, as if the
    // test runner had descheduled the diagnostic pass before spawning it.
    limits.before_describe_launch = Some(|start| {
        thread::sleep((start + RPC_TIMEOUT).saturating_duration_since(Instant::now()));
    });
    let result = run_with(
        host.clone(),
        DoctorOptions::default(),
        Arc::new(FakeLogs::default()),
        &limits,
    );
    assert_eq!(host.connects.load(Ordering::Relaxed), 1);
    assert!(result.document.daemon_answered);
}

fn execute(host: Fixture) -> DoctorRun {
    execute_with_limits(host, &fixture_limits())
}

fn execute_with_limits(host: Fixture, limits: &Limits) -> DoctorRun {
    run_with(
        Arc::new(host),
        DoctorOptions::default(),
        Arc::new(FakeLogs::default()),
        limits,
    )
}

fn ids(result: &DoctorRun) -> Vec<&str> {
    result
        .document
        .findings
        .iter()
        .map(|f| f.id.as_str())
        .collect()
}

fn finding<'a>(result: &'a DoctorRun, id: &str) -> &'a Finding {
    result
        .document
        .findings
        .iter()
        .find(|f| f.id == id)
        .unwrap_or_else(|| panic!("missing {id}: {:?}", ids(result)))
}

fn module(id: &str, state: &str) -> SupervisorEntry {
    SupervisorEntry::new(id, state, true, true, Health::Ok)
}

fn usage(user: u64, system: u64, memory_bytes: u64) -> ResourceUsage {
    ResourceUsage {
        cpu_user: Duration::from_millis(user),
        cpu_system: Duration::from_millis(system),
        memory_bytes,
        memory_kind: MemoryKind::ResidentSet,
        swap_bytes: None,
    }
}

#[test]
fn healthy_stack_uses_inventory_and_one_connection() {
    let mut host = Fixture::new("doctor-healthy");
    host.versions.insert("ck".into(), "1.2.3".into());
    let mut healthy = module("agent", "running");
    healthy.last_probe_ms = Some(42);
    host.set_daemon(FakeDaemon {
        resource_row: true,
        modules: vec![healthy],
        ..FakeDaemon::default()
    });
    let host = Arc::new(host);
    let start = Instant::now();
    let result = run_with(
        host.clone(),
        DoctorOptions::default(),
        Arc::new(FakeLogs::default()),
        &Limits::default(),
    );
    assert!(start.elapsed() < Duration::from_secs(20));
    assert_eq!(result.exit_code, 0);
    assert!(result.document.daemon_answered);
    assert_eq!(host.connects.load(Ordering::Relaxed), 1);
    assert_eq!(
        finding(&result, "install.versions").evidence,
        json!({"binaries":{"ck":"1.2.3"}})
    );
    assert_eq!(
        finding(&result, "daemon.resources").evidence["memory_bytes"],
        8
    );
    assert!(result
        .document
        .findings
        .iter()
        .all(|f| f.severity == Severity::Info));
    assert_eq!(
        *host.daemon.caps.lock().unwrap(),
        [("agent".into(), 20, 64 * 1024)]
    );
    assert_eq!(result.log_tails[0].status, TailStatus::Captured);
    assert_eq!(
        serde_json::to_value(&result.document).unwrap()["schema"],
        "ck.doctor.report/v1"
    );
}

#[test]
fn install_prefers_command_layout() {
    for legacy_ck in [false, true] {
        let mut host = Fixture::new("doctor-command-layout");
        host.legacy_ck = legacy_ck;
        let (host, result) = check_layout(host);
        assert_eq!(result.exit_code, 0);
        assert!(!ids(&result).contains(&"install.binary_missing"));
        assert!(!ids(&result).contains(&"install.old_layout"));
        assert_layout_reads(&host, &["cmd"]);
    }
}

#[test]
fn install_reports_legacy_layout_as_info() {
    let mut host = Fixture::new("doctor-legacy-layout");
    host.binaries = Ok([true, false]);
    host.legacy_ck = true;
    let (host, result) = check_layout(host);
    assert_eq!(result.exit_code, 0);
    assert!(!ids(&result).contains(&"install.binary_missing"));
    let legacy = finding(&result, "install.old_layout");
    assert_eq!(legacy.owner, "ck");
    assert_eq!(legacy.severity, Severity::Info);
    let path = host.data.join("bin").join(ck_file_name());
    assert_eq!(legacy.evidence, json!({"path": path}));
    assert!(legacy.summary.contains(&*path.to_string_lossy()));
    assert_layout_reads(&host, &["cmd", "bin"]);
}

#[test]
fn install_reports_ck_missing_only_after_checking_both_layouts() {
    let mut host = Fixture::new("doctor-no-command-layout");
    host.binaries = Ok([true, false]);
    let (host, result) = check_layout(host);
    assert_eq!(result.exit_code, 1);
    assert!(!ids(&result).contains(&"install.old_layout"));
    let missing = result
        .document
        .findings
        .iter()
        .filter(|finding| finding.id == "install.binary_missing")
        .collect::<Vec<_>>();
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].evidence["binary"], "ck");
    assert_layout_reads(&host, &["cmd", "bin"]);
}

fn check_layout(host: Fixture) -> (Arc<Fixture>, DoctorRun) {
    let host = Arc::new(host);
    let result = run_with(
        host.clone(),
        DoctorOptions::default(),
        Arc::new(FakeLogs::default()),
        &fixture_limits(),
    );
    (host, result)
}

fn ck_file_name() -> String {
    format!("ck{}", std::env::consts::EXE_SUFFIX)
}

fn assert_layout_reads(host: &Fixture, directories: &[&str]) {
    let name = ck_file_name();
    let checks = host.binary_checks.lock().unwrap();
    let checked_ck = checks
        .iter()
        .filter(|path| path.ends_with(&name))
        .cloned()
        .collect::<Vec<_>>();
    let expected = directories
        .iter()
        .map(|directory| host.data.join(directory).join(&name))
        .collect::<Vec<_>>();
    assert_eq!(checked_ck, expected);
}

#[test]
fn install_missing_binaries_are_distinct_from_path_failure_and_unknown_version() {
    let mut host = Fixture::new("doctor-install-empty");
    host.binaries = Ok([false, false]);
    let result = execute(host);
    assert_eq!(result.exit_code, 1);
    assert_eq!(
        result
            .document
            .findings
            .iter()
            .filter(|f| f.id == "install.binary_missing")
            .map(|f| &f.evidence["binary"])
            .collect::<Vec<_>>(),
        [&json!("ck-subc"), &json!("ck")]
    );
    assert_eq!(
        finding(&result, "install.versions").evidence,
        json!({"binaries":{}})
    );

    let mut host = Fixture::new("doctor-install-invalid-path");
    host.binaries = Err("home is unavailable".into());
    host.versions.insert("ck".into(), "unknown".into());
    let result = execute(host);
    assert!(!ids(&result).contains(&"install.binary_missing"));
    assert_eq!(
        finding(&result, "install.check_failed").evidence["check"],
        "runtime_paths"
    );
    assert_eq!(
        finding(&result, "install.versions").evidence["binaries"]["ck"],
        "unknown"
    );
}

#[test]
fn unanswered_daemon_is_classified_without_believing_connection_pid() {
    for (state, pid, entry, id) in [
        (ManagerState::NotLoaded, None, None, "daemon.not_running"),
        (
            ManagerState::Running,
            Some(44),
            Some(ProcessEntry {
                pid: 44,
                start_time_ms: Some(5),
            }),
            "daemon.unreachable",
        ),
        (ManagerState::Running, Some(44), None, "daemon.unreachable"),
        (ManagerState::Running, None, None, "daemon.unreachable"),
    ] {
        let expected_entry = serde_json::to_value(&entry).unwrap();
        let mut host = Fixture::new("doctor-offline");
        host.answers = false;
        host.set_manager(state, MarkerState::Absent, pid);
        host.process = entry;
        let result = execute(host);
        assert_eq!(result.exit_code, 1);
        assert!(!result.document.daemon_answered);
        let evidence = &finding(&result, id).evidence;
        assert_eq!(evidence["connection_pid_hint"], std::process::id());
        assert_eq!(evidence["recorded_pid"], json!(pid));
        assert_eq!(evidence["process_entry"], expected_entry);
        assert_eq!(result.log_tails.len(), 0);
        assert_eq!(
            ids(&result)
                .iter()
                .filter(|id| **id == "module.daemon_unavailable")
                .count(),
            1
        );
    }

    let mut host = Fixture::new("doctor-manager-unknown");
    host.answers = false;
    host.manager = (Err("manager command failed".into()), MarkerState::Unknown);
    let result = execute(host);
    let unreachable = finding(&result, "daemon.unreachable");
    assert_eq!(unreachable.evidence["manager"], "unknown");
    assert_eq!(
        unreachable.evidence["manager_error"],
        "manager command failed"
    );
    assert!(!unreachable.summary.contains("prompt"));
    assert_eq!(
        finding(&result, "service.unknown").severity,
        Severity::Warning
    );
}

#[test]
fn offline_override_names_path_no_local_inference_or_logs_or_offer() {
    let mut host = Fixture::new("doctor-override");
    host.answers = false;
    host.set_manager(ManagerState::Running, MarkerState::Absent, Some(44));
    host.configured = Ok(vec!["module".into()]);
    let window = host.add_window(0, 1, WindowPhase::Update(Liveness::Gone));
    host.windows.get_mut().unwrap().push(window);
    let logs = Arc::new(FakeLogs::default());
    let path = host.scratch.path().join("named.json");
    let result = run_with(
        Arc::new(host),
        DoctorOptions {
            connection_override: Some(path.clone()),
            fix: false,
        },
        logs.clone(),
        &fixture_limits(),
    );
    let unreachable = finding(&result, "daemon.unreachable");
    assert!(unreachable.summary.contains(path.to_str().unwrap()));
    assert_eq!(unreachable.evidence["manager"], "unknown");
    assert_eq!(unreachable.evidence["recorded_pid"], Value::Null);
    assert_eq!(
        unreachable.evidence["connection_pid_hint"],
        std::process::id()
    );
    assert!(result.document.offer.is_none());
    assert!(ids(&result).contains(&"window.interrupted"));
    assert!(logs.seen.lock().unwrap().is_empty());
    assert!(result.log_tails.is_empty());
}

#[cfg(feature = "test-support")]
pub(crate) fn run_with_blocked_describe(
    observation: (Result<Job, String>, MarkerState),
) -> DoctorRun {
    let mut host = Fixture::new("doctor-describe-late");
    let gate = Arc::new(Gate::default());
    host.describe_gate = Some(gate.clone());
    host.manager = observation;
    let result = execute_with_limits(
        host,
        &Limits {
            describe: RPC_TIMEOUT,
            ..fixture_limits()
        },
    );
    // Unblock describe so its detached worker can exit.
    gate.open();
    result
}

#[test]
fn cpu_delta_uses_both_durations_and_a_strict_core_threshold() {
    for (user, system, cores, severity) in [
        (2500, 700, 1.0, None),
        (2501, 700, 3001.0 / 3000.0, Some(Severity::Warning)),
        (5500, 700, 2.0, Some(Severity::Warning)),
        (5501, 700, 6001.0 / 3000.0, Some(Severity::Error)),
    ] {
        let sample = cpu_delta(
            Some(usage(100, 100, 1)),
            Some(usage(user, system, 25)),
            Duration::from_secs(3),
        )
        .unwrap();
        let findings = resources(Ok(sample));
        assert!((findings[0].evidence["cores"].as_f64().unwrap() - cores).abs() < 1e-10);
        assert_eq!(findings[0].evidence["cpu_ms"], user + system - 200);
        assert_eq!(findings[0].evidence["elapsed_ms"], 3000);
        assert_eq!(findings[0].evidence["memory_bytes"], 25);
        assert_eq!(findings[0].evidence["memory_kind"], "resident_set");
        assert_eq!(
            findings.get(1).map(|f| (f.id.as_str(), f.severity)),
            severity.map(|s| ("daemon.cpu_high", s))
        );
    }
}

#[test]
fn cpu_does_not_measure_a_changed_pid_or_absent_row() {
    let mut host = Fixture::new("doctor-cpu-reuse");
    host.process_changed = true;
    host.set_daemon(FakeDaemon {
        resource_row: true,
        ..FakeDaemon::default()
    });
    let result = execute(host);
    assert_eq!(
        finding(&result, "daemon.resources").evidence["reason"],
        "process changed"
    );
    assert_eq!(
        finding(&result, "daemon.resources").evidence["cores"],
        Value::Null
    );
    let result = execute(Fixture::new("doctor-no-resource-row"));
    assert_eq!(
        finding(&result, "daemon.resources").evidence["reason"],
        "no resource row"
    );
}

#[test]
fn cpu_measures_the_same_birth_after_an_epoch_step() {
    let mut host = Fixture::new("doctor-cpu-clock-step");
    host.epoch_step = true;
    host.set_daemon(FakeDaemon {
        resource_row: true,
        ..FakeDaemon::default()
    });
    let result = execute(host);
    let evidence = &finding(&result, "daemon.resources").evidence;
    assert_eq!(evidence["reason"], Value::Null);
    assert_eq!(evidence["cpu_ms"], 0);
}

#[test]
fn disk_uses_existing_ancestor_and_strict_space_limits() {
    for (free, severity) in [
        (5 << 30, None),
        ((5 << 30) - 1, Some(Severity::Warning)),
        (1 << 30, Some(Severity::Warning)),
        ((1 << 30) - 1, Some(Severity::Error)),
    ] {
        let mut host = Fixture::new("doctor-disk-space");
        let parent = host.data.clone();
        host.data = parent.join("absent/nested");
        host.free_space = Some(free);
        let host = Arc::new(host);
        let run = run_with(
            host.clone(),
            DoctorOptions::default(),
            Arc::new(FakeLogs::default()),
            &fixture_limits(),
        );
        assert_eq!(host.disk_path.lock().unwrap().as_ref(), Some(&parent));
        assert!(!host.data.exists());
        assert_eq!(
            run.document
                .findings
                .iter()
                .find(|f| f.id == "disk.low_space")
                .map(|f| f.severity),
            severity
        );
    }
    let mut host = Fixture::new("doctor-disk-no-space-reading");
    host.free_space = None;
    assert_eq!(
        finding(&execute(host), "disk.check_failed").evidence["check"],
        "disk_space"
    );
}

#[cfg(unix)]
#[test]
fn unix_permissions_find_only_access_that_matters_without_repairing() {
    use std::os::unix::fs::PermissionsExt;
    let host = Arc::new(Fixture::new("doctor-modes"));
    let run = host.scratch.path().join("run");
    let settings = host.scratch.path().join("settings");
    let file = run.join("connection.json");
    fs::create_dir(&run).unwrap();
    fs::create_dir(&settings).unwrap();
    fs::write(&file, "test").unwrap();
    for (path, mode) in [
        (&run, 0o770),
        (&file, 0o640),
        (&host.data, 0o775),
        (&settings, 0o772),
    ] {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }
    let pass = || {
        run_with(
            host.clone(),
            DoctorOptions::default(),
            Arc::new(FakeLogs::default()),
            &fixture_limits(),
        )
    };
    let result = pass();
    let permissions = result
        .document
        .findings
        .iter()
        .filter(|f| f.id == "disk.folder_permissions")
        .collect::<Vec<_>>();
    assert_eq!(
        permissions.iter().map(|f| f.severity).collect::<Vec<_>>(),
        [
            Severity::Error,
            Severity::Error,
            Severity::Warning,
            Severity::Warning
        ]
    );
    assert_eq!(
        permissions
            .iter()
            .map(|f| f.evidence["mode"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [0o770, 0o640, 0o775, 0o772]
    );
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o640
    );
    fs::set_permissions(&run, fs::Permissions::from_mode(0o1777)).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    let result = pass();
    assert_eq!(
        result
            .document
            .findings
            .iter()
            .filter(|f| f.id == "disk.folder_permissions")
            .count(),
        2
    );
    assert_eq!(
        fs::metadata(run).unwrap().permissions().mode() & 0o7777,
        0o1777
    );
}

#[cfg(windows)]
#[test]
fn windows_permissions_never_read_a_dacl() {
    let result = execute(Fixture::new("doctor-windows-permissions"));
    let disk = result
        .document
        .findings
        .iter()
        .filter(|f| f.layer == Layer::Disk)
        .collect::<Vec<_>>();
    assert_eq!(disk.len(), 1);
    assert_eq!(disk[0].id, "permissions.not_checked_on_windows");
    assert_eq!(disk[0].severity, Severity::Info);
}

#[test]
fn newest_readable_window_follows_every_table_row() {
    for (phase, age, id, severity, liveness, offer) in [
        (
            WindowPhase::Update(Liveness::Gone),
            0,
            "window.interrupted",
            Severity::Error,
            Some("gone"),
            true,
        ),
        (
            WindowPhase::Revert(Liveness::Gone),
            0,
            "window.revert_interrupted",
            Severity::Error,
            Some("gone"),
            true,
        ),
        (
            WindowPhase::Revert(Liveness::Alive),
            0,
            "window.reverting",
            Severity::Info,
            Some("alive"),
            false,
        ),
        (
            WindowPhase::Revert(Liveness::Unreadable),
            0,
            "window.reverting",
            Severity::Warning,
            Some("unreadable"),
            false,
        ),
        (
            WindowPhase::Update(Liveness::Alive),
            0,
            "window.update_in_progress",
            Severity::Info,
            Some("alive"),
            false,
        ),
        (
            WindowPhase::Update(Liveness::Unreadable),
            0,
            "window.update_in_progress",
            Severity::Info,
            Some("unreadable"),
            false,
        ),
        (
            WindowPhase::Empty,
            0,
            "window.empty",
            Severity::Info,
            None,
            false,
        ),
        (
            WindowPhase::Idle,
            0,
            "window.not_kept",
            Severity::Warning,
            None,
            true,
        ),
        (
            WindowPhase::Idle,
            24 * 60 * 60 * 1000,
            "window.not_kept",
            Severity::Info,
            None,
            false,
        ),
    ] {
        let host = Fixture::new("doctor-window-state");
        let row = host.add_window(age, 1, phase);
        let window_id = row.0.as_ref().unwrap().id.clone();
        host.windows.lock().unwrap().push(row);
        let result = execute(host);
        let finding = finding(&result, id);
        assert_eq!(finding.owner, "ck");
        assert_eq!(finding.severity, severity);
        assert_eq!(finding.evidence["owner_liveness"], json!(liveness));
        assert_eq!(finding.evidence["window_id"], window_id);
        assert_eq!(
            result.document.offer.as_ref().map(|o| o.command.clone()),
            offer.then(|| format!("ck window revert {window_id}"))
        );
        if id == "window.interrupted" {
            assert!(finding.summary.contains("step 1 of 1"));
            assert_eq!(finding.evidence["unfinished_index"], 1);
            assert_eq!(finding.evidence["steps"][0]["action"], "file");
        }
        if id == "window.revert_interrupted" {
            assert_eq!(finding.evidence["state"], "revert_interrupted");
        }
        if id == "window.empty" {
            assert_eq!(finding.evidence["step_count"], 0);
            assert_eq!(finding.evidence["unfinished_index"], Value::Null);
        }
    }
}

#[test]
fn unreadable_windows_anywhere_block_start_and_withhold_offer_without_hiding_readable_rows() {
    let mut host = Fixture::new("doctor-unreadable-windows");
    host.set_manager(ManagerState::Loaded, MarkerState::Present, None);
    let candidate = host.add_window(0, 1, WindowPhase::Idle);
    *host.windows.get_mut().unwrap() = vec![
        (Err(("newest".into(), "truncated".into())), None),
        candidate,
        (Err(("older".into(), "inaccessible".into())), None),
    ];
    let result = execute(host);
    assert!(result.document.offer.is_none());
    assert_eq!(
        finding(&result, "window.not_kept").severity,
        Severity::Warning
    );
    assert_eq!(
        finding(&result, "service.stopped").evidence["start_blocked_by"],
        json!({"id":"window.unreadable", "window_id":"newest"})
    );
    let failures = result
        .document
        .findings
        .iter()
        .filter(|f| f.id == "window.unreadable")
        .map(|f| &f.evidence)
        .collect::<Vec<_>>();
    assert_eq!(
        failures,
        [
            &json!({"window_id":"newest", "error":"truncated"}),
            &json!({"window_id":"older", "error":"inaccessible"})
        ]
    );
}

#[test]
fn absent_data_folder_skips_discovery_and_scan_failure_becomes_unreadable_windows_row() {
    let host = Fixture::new("doctor-scan-windows");
    assert!(read_windows(Ok(host.data.join("absent")), || panic!(
        "must not discover Windows for an absent data folder"
    ))
    .is_empty());
    *host.windows.lock().unwrap() =
        read_windows(Ok(host.data.clone()), || Err("scan failed".into()));
    let result = execute(host);
    assert_eq!(
        finding(&result, "window.unreadable").evidence,
        json!({"window_id":"windows", "error":"scan failed"})
    );
}

#[test]
fn stopped_service_points_to_revert_blockers_but_not_interrupted_updates() {
    for (phase, id) in [
        (
            WindowPhase::Revert(Liveness::Gone),
            "window.revert_interrupted",
        ),
        (WindowPhase::Revert(Liveness::Alive), "window.reverting"),
    ] {
        let mut host = Fixture::new("doctor-older-blocker");
        host.set_manager(ManagerState::Loaded, MarkerState::Present, None);
        let newer = host.add_window(0, 1, WindowPhase::Closed);
        let older = host.add_window(1000, 2, phase);
        *host.windows.get_mut().unwrap() = vec![newer, older];
        let result = execute(host);
        assert!(result.document.offer.is_none());
        assert!(ids(&result).contains(&id));
        assert_eq!(
            finding(&result, "service.stopped").evidence["start_blocked_by"]["id"],
            id
        );
    }
    let mut host = Fixture::new("doctor-start-interrupted");
    host.set_manager(ManagerState::Loaded, MarkerState::Present, None);
    let row = host.add_window(0, 1, WindowPhase::Update(Liveness::Gone));
    host.windows.get_mut().unwrap().push(row);
    let result = execute(host);
    assert!(finding(&result, "service.stopped")
        .summary
        .ends_with("`ck stack start`"));
    assert!(result.document.offer.is_some());
}

#[test]
fn stale_marker_is_info_and_a_marker_left_by_a_revert_blocks_start_as_needed() {
    let mut host = Fixture::new("doctor-stale-marker");
    host.set_manager(ManagerState::Running, MarkerState::Stale, Some(44));
    assert_eq!(
        finding(&execute(host), "service.stale_marker").severity,
        Severity::Info
    );

    let mut host = Fixture::new("doctor-revert-marker");
    host.answers = false;
    host.set_manager(ManagerState::NotLoaded, MarkerState::Present, None);
    let row = host.add_window(0, 1, WindowPhase::Revert(Liveness::Gone));
    host.windows.get_mut().unwrap().push(row);
    let result = execute(host);
    assert_eq!(
        finding(&result, "service.stopped").evidence["start_blocked_by"]["id"],
        "window.revert_interrupted"
    );
    assert!(ids(&result).contains(&"daemon.not_running"));
}

#[test]
fn fix_rechecks_only_after_a_revert_that_changes_something() {
    for (outcome, expected_exit, passes, outcome_text) in [
        (RevertOutcome::Reverted, 0, 2, "reverted"),
        (RevertOutcome::StoppedPartWay, 1, 2, "stopped_part_way"),
        (
            RevertOutcome::RefusedBeforeChange,
            2,
            1,
            "refused_before_change",
        ),
    ] {
        let mut host = Fixture::new("doctor-fix-outcome");
        host.outcome = outcome;
        let row = host.add_window(0, 1, WindowPhase::Update(Liveness::Gone));
        let window_id = row.0.as_ref().unwrap().id.clone();
        host.windows.get_mut().unwrap().push(row);
        let host = Arc::new(host);
        let result = run_with(
            host.clone(),
            DoctorOptions {
                fix: true,
                ..DoctorOptions::default()
            },
            Arc::new(FakeLogs::default()),
            &fixture_limits(),
        );
        assert_eq!(*host.reverts.lock().unwrap(), [window_id]);
        assert_eq!(host.connects.load(Ordering::Relaxed), passes);
        assert_eq!(result.exit_code, expected_exit);
        assert_eq!(result.document.ok, passes == 2);
        let report = serde_json::to_value(&result.document).unwrap();
        assert_eq!(report["fix"]["result"]["schema"], "ck.window.result/v1");
        assert_eq!(report["fix"]["result"]["outcome"], outcome_text);
        assert_eq!(report["fix"]["skipped"], Value::Null);
        let fix = result.document.fix.as_ref().unwrap();
        assert!(fix.result.as_ref().unwrap().human().contains(outcome_text));
        assert!(fix
            .findings_before
            .iter()
            .any(|f| f.id == "window.interrupted"));
        if passes == 1 {
            assert_eq!(report["findings"], report["fix"]["findings_before"]);
            assert_eq!(report["fix"]["result"]["message"], "Approval declined");
        }
    }
}

#[test]
fn fix_never_touches_manager_after_deadline_or_with_an_override() {
    let mut host = Fixture::new("doctor-fix-overdue");
    let manager = Arc::new(Gate::default());
    host.manager_gate = Some(manager.clone());
    let row = host.add_window(0, 1, WindowPhase::Update(Liveness::Gone));
    host.windows.get_mut().unwrap().push(row);
    let host = Arc::new(host);
    let result = run_with(
        host.clone(),
        DoctorOptions {
            fix: true,
            ..DoctorOptions::default()
        },
        Arc::new(FakeLogs::default()),
        &Limits {
            budget_override: Some((Budget::Manager, LOCAL_TIMEOUT)),
            ..fixture_limits()
        },
    );
    manager.open();
    assert!(host.reverts.lock().unwrap().is_empty());
    assert!(result.document.offer.is_some());
    assert_eq!(result.exit_code, 1);
    assert_eq!(
        finding(&result, "service.timed_out").severity,
        Severity::Error
    );
    assert_eq!(
        result.document.fix.unwrap().skipped,
        Some(FixSkip::ManagerObservationOverdue)
    );

    let result = run_with(
        host.clone(),
        DoctorOptions {
            fix: true,
            connection_override: Some("named.json".into()),
        },
        Arc::new(FakeLogs::default()),
        &fixture_limits(),
    );
    assert_eq!(host.connects.load(Ordering::Relaxed), 1);
    assert_eq!(result.exit_code, 2);
    assert!(result.document.findings.is_empty());
}

#[test]
fn fix_without_an_offer_does_not_revert_and_exits_by_findings() {
    let host = Arc::new(Fixture::new("doctor-no-fix-needed"));
    let result = run_with(
        host.clone(),
        DoctorOptions {
            fix: true,
            ..DoctorOptions::default()
        },
        Arc::new(FakeLogs::default()),
        &fixture_limits(),
    );
    assert!(host.reverts.lock().unwrap().is_empty());
    assert_eq!(result.exit_code, 0);
    assert_eq!(
        result.document.fix.unwrap().skipped,
        Some(FixSkip::NothingToFix)
    );
}

#[test]
fn blocked_window_and_cpu_workers_are_abandoned_at_their_own_deadlines() {
    let mut host = Fixture::new("doctor-blocked-host");
    let gate = Arc::new(Gate::default());
    host.window_gate = Some(gate.clone());
    host.cpu_gate = Some(gate.clone());
    host.set_daemon(FakeDaemon {
        resource_row: true,
        ..FakeDaemon::default()
    });
    let start = Instant::now();
    let result = execute_with_limits(
        host,
        &Limits {
            window: RPC_TIMEOUT,
            budget_override: Some((Budget::Cpu, LOCAL_TIMEOUT)),
            ..fixture_limits()
        },
    );
    gate.open();
    assert!(start.elapsed() < Duration::from_secs(20));
    assert!(ids(&result).contains(&"daemon.timed_out"));
    assert!(ids(&result).contains(&"window.timed_out"));
    assert_eq!(
        finding(&result, "daemon.resources").evidence["reason"],
        "CPU sample timed out"
    );
}

#[test]
fn list_timeout_stops_module_calls_but_health_errors_do_not_erase_list_verdicts() {
    let mut host = Fixture::new("doctor-list-timeout");
    let gate = Arc::new(Gate::default());
    host.set_daemon(FakeDaemon {
        list_gate: Some(gate.clone()),
        modules: vec![module("m", "running")],
        ..FakeDaemon::default()
    });
    let daemon = host.daemon.clone();
    let result = execute_with_limits(
        host,
        &Limits {
            budget_override: Some((Budget::List, RPC_TIMEOUT)),
            ..fixture_limits()
        },
    );
    gate.open();
    let timeout = finding(&result, "module.timed_out");
    assert_eq!((&*timeout.owner, timeout.severity), ("ck", Severity::Error));
    assert_eq!(timeout.evidence["check"], "supervisor.list");
    assert_eq!(
        finding(&result, "daemon.resources").evidence["reason"],
        "supervisor.list timed out"
    );
    assert!(daemon
        .calls
        .lock()
        .unwrap()
        .iter()
        .all(|(check, _, _)| *check == "list" || *check == "health"));

    for health in [Reply::Failed, Reply::TimedOut] {
        let mut host = Fixture::new("doctor-health-failed");
        host.set_daemon(FakeDaemon {
            health,
            modules: vec![module("m", "failed")],
            ..FakeDaemon::default()
        });
        let result = execute(host);
        let failed = finding(&result, "module.check_failed");
        assert_eq!((&*failed.owner, failed.severity), ("ck", Severity::Warning));
        assert_eq!(failed.evidence["check"], "supervisor.health");
        assert_eq!(finding(&result, "module.failed").owner, "m");
    }
}

#[test]
fn module_workers_do_not_start_requests_after_the_pass_cutoff() {
    let mut host = Fixture::new("doctor-many-modules");
    host.set_daemon(FakeDaemon {
        modules: (0..27)
            .map(|n| module(&format!("module-{n:02}"), "running"))
            .collect(),
        job_delay: Duration::from_millis(2_750),
        ..FakeDaemon::default()
    });
    let daemon = host.daemon.clone();
    let start = Instant::now();
    let result = run_with(
        Arc::new(host),
        DoctorOptions::default(),
        Arc::new(FakeLogs::default()),
        &Limits::default(),
    );
    assert!(start.elapsed() < Duration::from_secs(20));
    assert_eq!(result.log_tails.len(), 27);
    let calls = daemon.calls.lock().unwrap();
    assert!(calls
        .iter()
        .all(|(_, _, began)| *began < start + Duration::from_secs(15)));
    let caps = daemon.caps.lock().unwrap();
    assert!(caps
        .iter()
        .all(|(_, lines, bytes)| *lines == 20 && *bytes == 64 * 1024 / 27));
    assert!(
        caps.len() < 27,
        "test load must exceed the allotted job budget"
    );
    for i in 0..27 {
        let module = format!("module-{i:02}");
        assert!(
            calls
                .iter()
                .any(|(check, id, _)| *check == "terminals" && id == &module)
                || result
                    .document
                    .findings
                    .iter()
                    .any(|f| f.owner == module && f.id == "module.timed_out")
        );
    }
}

#[tokio::test]
async fn dropped_connection_discards_in_flight_reply_and_never_runs_later_reads() {
    let fence = ConnectionFence::default();
    let deadline = Instant::now() + Duration::from_secs(2);
    assert!(matches!(
        fence.read(deadline, async { Ok::<_, String>(7) }).await,
        Ok(7)
    ));
    let in_flight = fence
        .read(deadline, async {
            fence.drop_observed();
            Ok::<_, String>(8)
        })
        .await;
    assert!(matches!(in_flight, Err(RpcError::ConnectionLost)));
    let run_next = || -> Result<u32, String> { panic!("a subsequent RPC must not be polled") };
    let next = fence.read(deadline, async { run_next() }).await;
    assert!(matches!(next, Err(RpcError::ConnectionLost)));
}

#[test]
fn lost_connection_after_describe_gets_one_error_not_a_reconnect() {
    let mut host = Fixture::new("doctor-disconnect");
    host.set_daemon(FakeDaemon {
        list: Reply::Lost,
        health: Reply::Lost,
        ..FakeDaemon::default()
    });
    let host = Arc::new(host);
    let result = run_with(
        host.clone(),
        DoctorOptions::default(),
        Arc::new(FakeLogs::default()),
        &fixture_limits(),
    );
    assert_eq!(host.connects.load(Ordering::Relaxed), 1);
    assert!(result.document.daemon_answered);
    assert_eq!(result.exit_code, 1);
    assert_eq!(
        ids(&result)
            .iter()
            .filter(|id| **id == "daemon.connection_lost")
            .count(),
        1
    );
    assert_eq!(
        finding(&result, "daemon.connection_lost").evidence,
        json!({"step":"supervisor.list"})
    );
    assert_eq!(
        ids(&result)
            .iter()
            .filter(|id| **id == "module.check_failed")
            .count(),
        2
    );
    assert!(result.log_tails.is_empty());
}

#[test]
fn all_module_states_are_mapped_without_inventing_health_for_future_states() {
    for (state, code, severity, exit) in [
        ("running", None, Severity::Info, 0),
        ("failed", Some("module.failed"), Severity::Error, 1),
        (
            "unresponsive",
            Some("module.unresponsive"),
            Severity::Error,
            1,
        ),
        ("starting", Some("module.starting"), Severity::Info, 0),
        ("restarting", Some("module.restarting"), Severity::Info, 0),
        ("draining", Some("module.draining"), Severity::Info, 0),
        ("stopped", None, Severity::Info, 0),
        ("disabled", None, Severity::Info, 0),
        (
            "future_state",
            Some("module.unknown_state"),
            Severity::Warning,
            0,
        ),
    ] {
        let mut row = module("sample", state);
        if matches!(state, "stopped" | "disabled") {
            row.enabled = false;
        }
        if state == "future_state" {
            row.health = Health::Failing;
            row.last_probe_ms = Some(42);
            row.live = false;
        }
        // Classify completed data: slow RPC workers must not affect how a state maps.
        let tail = LogTailRecord::empty("sample", TailSource::StderrTail, TailStatus::Captured);
        let mut pass = Pass::default();
        pass.findings
            .extend(module_checks::classify(&row, None, None, &tail, 0));
        let result = pass.report(None, None, None);
        assert_eq!(result.exit_code, exit, "state={state}: {:?}", ids(&result));
        let findings = &result.document.findings;
        assert_eq!(findings.len(), usize::from(code.is_some()), "state={state}");
        if let Some(code) = code {
            assert_eq!(
                (
                    &*findings[0].id,
                    findings[0].layer,
                    findings[0].severity,
                    &*findings[0].owner
                ),
                (code, Layer::Module, severity, "sample"),
                "state={state}"
            );
            assert_eq!(findings[0].evidence["state"], state, "state={state}");
            if state == "future_state" {
                assert!(
                    findings[0].summary.contains("\"future_state\""),
                    "state={state}"
                );
            }
        }
    }
}

fn exit_entry(kind: Option<TerminalExitKind>, disposition: TerminalDisposition) -> TerminalEntry {
    TerminalEntry {
        daemon_incarnation: None,
        at_ms: now_ms() - 100,
        exit_code: Some(1),
        exit_signal: None,
        disposition,
        exit_kind: kind,
        disposition_detail: None,
    }
}

fn crash_exits() -> Vec<TerminalEntry> {
    vec![
        exit_entry(
            Some(TerminalExitKind::Crash),
            TerminalDisposition::Restarting
        );
        3
    ]
}

#[test]
fn crash_loop_counts_only_recent_crashes_and_windowed_restarts_and_keeps_tail() {
    let mut module = module("m", "running");
    module.restart_count = Some(3);
    module.max_restarts = Some(3);
    module.restart_window_secs = Some(600);
    let mut host = Fixture::new("doctor-crash-loop");
    let tail = StderrTail {
        capture: StderrCaptureState::Captured,
        entries: (0..10)
            .map(|n| StderrTailEntry::Line {
                text: format!("line {n}"),
                at_ms: Some(42),
                truncated: false,
            })
            .collect(),
        dropped_lines: 3,
    };
    host.set_daemon(FakeDaemon {
        modules: vec![module.clone()],
        exits: crash_exits(),
        tail_data: BTreeMap::from([("m".into(), tail)]),
        ..FakeDaemon::default()
    });
    let result = execute(host);
    let crash = finding(&result, "module.crash_loop");
    assert_eq!((&*crash.owner, crash.severity), ("m", Severity::Error));
    assert_eq!(crash.evidence["restart_window_secs"], 600);
    assert_eq!(crash.evidence["exits"].as_array().unwrap().len(), 3);
    assert_eq!(crash.evidence["exits"][2]["kind"], "crash");
    assert_eq!(
        crash.evidence["tail"]["entries"].as_array().unwrap().len(),
        5
    );
    assert_eq!(crash.evidence["tail"]["entries"][0]["text"], "line 5");
    assert_eq!(result.log_tails[0].entries.len(), 10);

    let not_crashes = [
        exit_entry(
            Some(TerminalExitKind::Clean),
            TerminalDisposition::Restarting,
        ),
        exit_entry(
            Some(TerminalExitKind::DeliberateSeverance),
            TerminalDisposition::Failed,
        ),
        exit_entry(
            Some(TerminalExitKind::Unknown("later".into())),
            TerminalDisposition::Failed,
        ),
        exit_entry(Some(TerminalExitKind::Crash), TerminalDisposition::Stopped),
        exit_entry(Some(TerminalExitKind::Crash), TerminalDisposition::Disabled),
        exit_entry(
            Some(TerminalExitKind::Crash),
            TerminalDisposition::DaemonShutdown,
        ),
    ];
    for exit in not_crashes {
        let mut host = Fixture::new("doctor-noncrash");
        host.set_daemon(FakeDaemon {
            modules: vec![module.clone()],
            exits: vec![exit; 3],
            ..FakeDaemon::default()
        });
        assert!(!ids(&execute(host)).contains(&"module.crash_loop"));
    }

    let mut host = Fixture::new("doctor-old-crash");
    let mut exits = crash_exits();
    exits[0].at_ms = now_ms().saturating_sub(600_001);
    host.set_daemon(FakeDaemon {
        modules: vec![module.clone()],
        exits,
        ..FakeDaemon::default()
    });
    assert!(!ids(&execute(host)).contains(&"module.crash_loop"));

    let mut host = Fixture::new("doctor-lifetime-restarts");
    module.restart_window_secs = None;
    host.set_daemon(FakeDaemon {
        modules: vec![module],
        exits: crash_exits(),
        ..FakeDaemon::default()
    });
    assert!(!ids(&execute(host)).contains(&"module.crash_loop"));
}

#[test]
fn admission_and_list_health_and_not_serving_use_the_right_inputs() {
    let mut host = Fixture::new("doctor-admission-health");
    let mut refused = module("a-refused", "failed");
    refused.admission_refused = Some("registration_channel_unsupported".into());
    let mut unhealthy = module("b-unhealthy", "running");
    unhealthy.health = Health::Failing;
    unhealthy.last_probe_ms = Some(10);
    let mut degraded = module("c-degraded", "running");
    degraded.health = Health::Degraded;
    degraded.last_probe_ms = Some(11);
    let mut unprobed = module("d-unprobed", "running");
    unprobed.health = Health::Failing;
    let mut not_serving = module("e-not-serving", "running");
    not_serving.live = false;
    not_serving.last_exit_ms = Some(30);
    not_serving.last_exit_code = Some(1);
    let health_data = [json!({"module_id":"b-unhealthy", "status":"ok", "detail":"stale cached reply", "consecutive_failures":2}),
        json!({"module_id":"d-unprobed", "status":"failing", "detail":"old failure", "consecutive_failures":5})]
        .into_iter().map(|value| serde_json::from_value(value).unwrap()).collect();
    host.set_daemon(FakeDaemon {
        modules: vec![refused, unhealthy, degraded, unprobed, not_serving],
        health_data,
        ..FakeDaemon::default()
    });
    let result = execute(host);
    assert!(finding(&result, "module.admission_refused")
        .summary
        .contains("lacks its support declaration"));
    assert!(!ids(&result).contains(&"module.failed"));
    let unhealthy = finding(&result, "module.unhealthy");
    assert_eq!(unhealthy.owner, "b-unhealthy");
    assert_eq!(unhealthy.evidence["health"], "failing");
    assert_eq!(unhealthy.evidence["detail"], "stale cached reply");
    assert_eq!(
        finding(&result, "module.degraded").severity,
        Severity::Warning
    );
    assert_eq!(
        finding(&result, "module.not_serving").evidence["last_exit"]["code"],
        1
    );
    assert!(result
        .document
        .findings
        .iter()
        .all(|f| f.owner != "d-unprobed"));
}

#[test]
fn intentionally_disabled_modules_ignore_stale_health_crashes_and_tail_timeouts() {
    let mut host = Fixture::new("doctor-disabled-modules");
    let disabled = |id, configured| {
        let mut row = module(id, "stopped");
        row.enabled = false;
        row.configured_enabled = Some(configured);
        row.health = Health::Degraded;
        row.last_probe_ms = Some(42);
        row.restart_count = Some(3);
        row.restart_window_secs = Some(600);
        row
    };
    host.set_daemon(FakeDaemon {
        modules: vec![disabled("operator", true), disabled("configured", false)],
        tail: Reply::TimedOut,
        exits: crash_exits(),
        ..FakeDaemon::default()
    });
    let daemon = host.daemon.clone();
    let result = execute(host);
    let module_findings = result
        .document
        .findings
        .iter()
        .filter(|f| f.layer == Layer::Module)
        .collect::<Vec<_>>();
    assert_eq!(module_findings.len(), 1);
    assert_eq!(module_findings[0].id, "module.stopped_by_operator");
    assert_eq!(module_findings[0].owner, "operator");
    assert_eq!(module_findings[0].severity, Severity::Info);
    assert!(daemon
        .calls
        .lock()
        .unwrap()
        .iter()
        .all(|(check, _, _)| *check != "terminals"));
    assert!(result
        .log_tails
        .iter()
        .all(|tail| tail.status == TailStatus::RequestTimedOut));
    assert_eq!(result.log_tails.len(), 2);
}

#[test]
fn stderr_records_distinguish_quiet_not_captured_timed_out_and_future_entries() {
    let future = serde_json::from_value(json!({
        "capture":{"state":"future_capture"}, "dropped_lines":3,
        "entries":[{"kind":"process_start"},{"kind":"future_entry","text":"not a line"},{"kind":"line","text":"hello","at_ms":42,"truncated":true}],
    })).unwrap();
    let mixed = module_checks::stderr_tail("future", future);
    assert_eq!(
        serde_json::to_value(&mixed).unwrap(),
        json!({
            "module":"future","source":"stderr_tail","status":"unknown","reason":"future_capture",
            "dropped_lines":3,"truncated":false,"entries":[{"kind":"process_start"}, {"kind":"unknown"},
                {"kind":"line","text":"hello","at_ms":42,"truncated":true}],
        })
    );
    let mut host = Fixture::new("doctor-tail-status");
    host.set_daemon(FakeDaemon {
        modules: ["quiet", "missing", "timeout", "failed"]
            .map(|id| module(id, "running"))
            .to_vec(),
        tail_overrides: BTreeMap::from([
            ("timeout".into(), Reply::TimedOut),
            ("failed".into(), Reply::Failed),
        ]),
        tail_data: BTreeMap::from([(
            "missing".into(),
            StderrTail {
                capture: StderrCaptureState::NotCaptured {
                    reason: "no reader".into(),
                },
                entries: Vec::new(),
                dropped_lines: 0,
            },
        )]),
        ..FakeDaemon::default()
    });
    let result = execute(host);
    assert!(result
        .document
        .findings
        .iter()
        .all(|f| f.layer != Layer::Module));
    let statuses = result
        .log_tails
        .iter()
        .map(|tail| (tail.module.as_str(), tail.status))
        .collect::<Vec<_>>();
    assert_eq!(
        statuses,
        [
            ("failed", TailStatus::RequestFailed),
            ("missing", TailStatus::NotCaptured),
            ("quiet", TailStatus::Captured),
            ("timeout", TailStatus::RequestTimedOut)
        ]
    );
    assert_eq!(result.log_tails[0].reason.as_deref(), Some("read failed"));
    assert_eq!(result.log_tails[1].reason.as_deref(), Some("no reader"));
    assert_eq!(result.log_tails[2].dropped_lines, Some(0));
    assert_eq!(result.log_tails[3].dropped_lines, None);
    assert!(result.log_tails.iter().all(|tail| tail.entries.is_empty()));
}

#[test]
fn malformed_offline_config_reports_an_error_reading_it() {
    let mut host = Fixture::new("doctor-malformed-config");
    host.answers = false;
    let config = host.data.join("subc.jsonc");
    fs::write(&config, "{ modules: [").unwrap();
    let error = subc_daemon::daemon_config::load(&config)
        .unwrap_err()
        .to_string();
    host.configured = Err(error.clone());
    let result = execute(host);
    assert_eq!(
        finding(&result, "module.check_failed").evidence["message"],
        error
    );
    assert!(result.log_tails.is_empty());
    assert_eq!(
        ids(&result)
            .iter()
            .filter(|id| **id == "module.daemon_unavailable")
            .count(),
        1
    );
}

#[test]
fn offline_preserves_truncated_reads_and_records_modules_left_by_deadline() {
    use module_checks::{offline, OfflineLogs};

    let mut host = Fixture::new("doctor-log-fallback-cutoff");
    host.configured = Ok(vec!["slow".into(), "after".into(), "z-not-started".into()]);
    let gate = Arc::new(Gate::default());
    let (entered, started) = mpsc::channel();
    let logs = Arc::new(FakeLogs {
        block_at: Some((entered, gate.clone())),
        ..FakeLogs::default()
    });
    let worker = OfflineLogs::start(Arc::new(host), logs.clone(), FAKE_WAIT_CAP);
    // When the slow read starts, the first tail is already queued for collection.
    started.recv_timeout(FAKE_WAIT_CAP).unwrap();
    let result = offline(Some(worker.limit_collection_for_test(LOCAL_TIMEOUT)));
    gate.open();
    let finding = |id| result.findings.iter().find(|row| row.id == id).unwrap();
    assert_eq!(finding("module.timed_out").owner, "ck");
    assert_eq!(result.tails.len(), 3);
    let first = &result.tails[0];
    assert_eq!(
        (&*first.module, first.source, first.status),
        ("after", TailSource::LogFile, TailStatus::Read)
    );
    assert_eq!(
        first.reason.as_deref(),
        Some("run folder could not be resolved")
    );
    assert!(first.truncated);
    assert_eq!(first.entries.len(), 20);
    assert_eq!(result.tails[1].status, TailStatus::NotRead);
    assert_eq!(result.tails[2].status, TailStatus::NotRead);
    assert_eq!(*logs.seen.lock().unwrap(), ["after", "slow"]);
    assert_eq!(
        finding("module.daemon_unavailable").evidence["tails"][0]["entries"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    assert_eq!(
        serde_json::to_value(first).unwrap()["entries"][19],
        json!({"kind":"line", "text":"line 19", "truncated":false})
    );
}

#[cfg(target_os = "macos")]
#[test]
fn mac_signature_probe_kills_and_reaps_at_its_budget_and_refuses_late_launch() {
    use std::process::Command;
    let probes = stack_checks::SignatureProbes::default();
    let start = Instant::now();
    let deadline = start + Duration::from_secs(5);
    let started = probes.clone();
    let stuck = thread::spawn(move || started.run(Command::new("/bin/sleep").arg("30"), deadline));
    let pid = wait_for_probe_pid(&probes);
    assert!(probes
        .run(&mut Command::new("/usr/bin/true"), deadline)
        .unwrap());
    assert_eq!(
        stuck.join().unwrap().unwrap_err(),
        "signature probe timed out"
    );
    assert!((Duration::from_secs(3)..Duration::from_secs(5)).contains(&start.elapsed()));
    assert!(subc_os::Process::open(pid).unwrap().is_none());
    let refused = probes
        .run(&mut Command::new("/bin/sleep"), start)
        .unwrap_err();
    assert!(refused.contains("could not start"));
    probes.close();
    assert!(probes
        .run(&mut Command::new("/usr/bin/true"), deadline)
        .is_err());

    let probes = stack_checks::SignatureProbes::default();
    let active = probes.clone();
    let worker = thread::spawn(move || {
        active.run(
            Command::new("/bin/sleep").arg("30"),
            Instant::now() + Duration::from_secs(5),
        )
    });
    let pid = wait_for_probe_pid(&probes);
    probes.close();
    assert!(worker.join().unwrap().is_err());
    assert!(subc_os::Process::open(pid).unwrap().is_none());
}

#[cfg(target_os = "macos")]
fn wait_for_probe_pid(probes: &stack_checks::SignatureProbes) -> u32 {
    (0..200)
        .find_map(|_| {
            probes.active_pid().or_else(|| {
                thread::sleep(Duration::from_millis(5));
                None
            })
        })
        .expect("the signature probe should have started")
}

#[test]
fn blocked_install_and_disk_checks_emit_their_layer_deadlines() {
    let mut host = Fixture::new("doctor-local-deadlines");
    let gate = Arc::new(Gate::default());
    host.install_gate = Some(gate.clone());
    host.disk_gate = Some(gate.clone());
    let result = execute_with_limits(
        host,
        &Limits {
            local: LOCAL_TIMEOUT,
            ..fixture_limits()
        },
    );
    gate.open();
    assert_eq!(
        finding(&result, "install.timed_out").severity,
        Severity::Error
    );
    assert_eq!(finding(&result, "disk.timed_out").severity, Severity::Error);
    assert!(ids(&result).contains(&"install.versions"));
    assert!(ids(&result).contains(&"daemon.resources"));
}

#[test]
fn terminal_read_failure_or_timeout_is_module_owned_and_keeps_tail() {
    for (reply, id, severity) in [
        (Reply::Failed, "module.check_failed", Severity::Warning),
        (Reply::TimedOut, "module.timed_out", Severity::Error),
    ] {
        let mut host = Fixture::new("doctor-terminal-error");
        host.set_daemon(FakeDaemon {
            modules: vec![module("m", "running")],
            terminal: reply,
            ..FakeDaemon::default()
        });
        let result = execute(host);
        let finding = finding(&result, id);
        assert_eq!((&*finding.owner, finding.severity), ("m", severity));
        assert_eq!(finding.evidence["module"], "m");
        assert_eq!(finding.evidence["check"], "supervisor.terminals");
        assert_eq!(result.log_tails[0].status, TailStatus::Captured);
    }
}
