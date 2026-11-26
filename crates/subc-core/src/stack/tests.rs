use super::*;
use crate::doctor::{
    classify_service_and_daemon, run_with_blocked_describe, DescribeOutcome, Severity,
};
use crate::setup::runtime::CommandResult;
use cortexkit_test_support::ScratchDir;
use serde_json::{json, Value};
use std::{cell::Cell, collections::VecDeque};
use subc_client_rs::SupervisorListResponse;
use subc_control::SupervisorHealthEntry;
use subc_daemon::{ExecutableIdentity, RecordReadError};

use RuntimePlatform::{Linux, Macos, Windows};
const PLATFORMS: [RuntimePlatform; 3] = [Macos, Linux, Windows];
const SHORT: Duration = Duration::from_millis(250);

#[derive(Default)]
struct FakeClock(Cell<Duration>);
impl Clock for FakeClock {
    fn elapsed(&self) -> Duration {
        self.0.get()
    }
    fn sleep(&self, duration: Duration) {
        self.0.set(self.elapsed() + duration);
    }
}

struct FakePresence {
    texts: Vec<String>,
    outcome: Outcome,
}
impl Presence for FakePresence {
    fn prompt(&mut self, text: &str) -> Outcome {
        self.texts.push(text.into());
        self.outcome.clone()
    }
}

struct Fixture {
    _tmp: ScratchDir,
    stack: Stack,
}
impl Fixture {
    fn new(platform: RuntimePlatform) -> Self {
        let tmp = ScratchDir::new("stack-command");
        let root = tmp.join("install");
        fs::create_dir(&root).unwrap();
        let paths = runtime::runtime_paths(platform, &root.join("bin"), tmp.path());
        fs::create_dir_all(paths.definition.parent().unwrap()).unwrap();
        fs::write(
            &paths.definition,
            runtime::desired_definition_checked(platform, &paths).unwrap(),
        )
        .unwrap();
        let stack = Stack {
            root,
            paths,
            platform,
            config: tmp.join("subc.jsonc"),
            connection: None,
        };
        Self { _tmp: tmp, stack }
    }
    fn marker(&self) -> PathBuf {
        self.stack.marker()
    }
    fn lock(&self, command: &str) -> LockedStack<'_> {
        self.stack.lock(command).unwrap()
    }
    fn runner<'a>(&'a self, clock: &'a FakeClock) -> FakeRunner<'a> {
        FakeRunner {
            fixture: self,
            clock,
            jobs: VecDeque::new(),
            default_job: Seen::Idle,
            fail_stop: false,
            fail_start: false,
            slow: None,
            calls: Vec::new(),
        }
    }
}

#[derive(Clone, Copy)]
enum Seen {
    Idle,
    Absent,
    Live(Option<u32>),
    Failure,
    Inconclusive,
}

struct FakeRunner<'a> {
    fixture: &'a Fixture,
    clock: &'a FakeClock,
    jobs: VecDeque<Seen>,
    default_job: Seen,
    fail_stop: bool,
    fail_start: bool,
    slow: Option<(&'static str, Duration)>,
    calls: Vec<String>,
}
impl FakeRunner<'_> {
    fn mutations(&self) -> Vec<&str> {
        self.calls
            .iter()
            .filter(|call| {
                call.contains(" bootout ")
                    || call.contains(" stop ")
                    || call.contains(" /End ")
                    || call.contains(" bootstrap ")
                    || call.contains(" kickstart ")
                    || call.contains(" start ")
                    || call.contains(" /Run ")
            })
            .map(String::as_str)
            .collect()
    }
}
impl CommandRunner for FakeRunner<'_> {
    fn announce_restart(&mut self, _: Option<&Path>) -> Result<(), String> {
        self.calls.push("shutdown intent restart".into());
        Ok(())
    }
    fn run(&mut self, program: &str, args: &[String]) -> Result<CommandResult, String> {
        let call = format!("{program} {}", args.join(" "));
        self.calls.push(call.clone());
        if let Some((verb, duration)) = self.slow {
            if args.iter().any(|arg| arg == verb) {
                self.clock.sleep(duration);
            }
        }
        let success = |stdout: String| {
            Ok(CommandResult {
                success: true,
                stdout,
                stderr: String::new(),
            })
        };
        let failure = |stderr: &str| {
            Ok(CommandResult {
                success: false,
                stdout: String::new(),
                stderr: stderr.into(),
            })
        };
        let negative = |stdout: &str| {
            Ok(CommandResult {
                success: false,
                stdout: stdout.into(),
                stderr: String::new(),
            })
        };
        let platform = self.fixture.stack.platform;
        let unit = platform.identifier();
        let launch = format!("gui/{}/{unit}", runtime::current_uid());
        let seen = self.jobs.front().copied().unwrap_or(self.default_job);
        match (
            platform,
            program,
            args.first().map(String::as_str),
            args.get(1).map(String::as_str),
        ) {
            (Macos, "launchctl", Some("print"), Some(target)) => {
                assert_eq!(args, ["print", &launch]);
                assert_eq!(target, launch);
                self.jobs.pop_front();
                match seen {
                    Seen::Idle => success("state = not running".into()),
                    Seen::Live(pid) => success(format!("state = running\n{}", pid.map_or_else(String::new, |pid| format!("pid = {pid}")))),
                    Seen::Absent => failure(&format!("Bad request.\nCould not find service \"{unit}\" in domain for user gui: 501")),
                    Seen::Failure => failure("could not contact launchd"),
                    Seen::Inconclusive => success("no state in this answer".into()),
                }
            }
            (Linux, "systemctl", Some("--user"), Some("is-enabled")) => {
                assert_eq!(args, ["--user", "is-enabled", unit]);
                match seen {
                    Seen::Absent => negative("not-found"),
                    Seen::Failure => {
                        self.jobs.pop_front();
                        failure("Failed to connect to bus")
                    }
                    Seen::Idle => negative("disabled"),
                    _ => success("enabled".into()),
                }
            }
            (Linux, "systemctl", Some("--user"), Some("is-active")) => {
                assert_eq!(args, ["--user", "is-active", unit]);
                self.jobs.pop_front();
                match seen {
                    Seen::Absent => negative("unknown"),
                    Seen::Idle => negative("inactive"),
                    Seen::Live(_) => success("active".into()),
                    Seen::Inconclusive => success("unexpected output".into()),
                    Seen::Failure => unreachable!(),
                }
            }
            (Linux, "systemctl", Some("--user"), Some("show")) => {
                assert_eq!(args, ["--user", "show", "-p", "FragmentPath", unit]);
                success(format!(
                    "FragmentPath={}",
                    self.fixture.stack.paths.definition.display()
                ))
            }
            (Windows, "schtasks.exe", Some("/Query"), _) => {
                assert_eq!(args, ["/Query", "/TN", unit, "/FO", "LIST"]);
                self.jobs.pop_front();
                match seen {
                    Seen::Idle => {
                        success("TaskName:  example\r\nStatus:                 Ready\r\n".into())
                    }
                    Seen::Live(_) => success("Status:                 Running\r\n".into()),
                    Seen::Absent => failure("ERROR: The system cannot find the file specified."),
                    Seen::Failure => failure("ERROR: Access is denied."),
                    Seen::Inconclusive => success("TaskName:  example\r\n".into()),
                }
            }
            _ if call == stop_call(&self.fixture.stack) => {
                if self.fail_stop {
                    failure("cannot stop")
                } else {
                    success(String::new())
                }
            }
            _ if call == start_call(&self.fixture.stack, false)
                || call == start_call(&self.fixture.stack, true) =>
            {
                if self.fail_start {
                    failure("cannot start")
                } else {
                    success(String::new())
                }
            }
            _ => panic!("unexpected manager command: {call}"),
        }
    }
}

fn stop_call(stack: &Stack) -> String {
    let unit = stack.platform.identifier();
    match stack.platform {
        Macos => format!("launchctl bootout gui/{}/{unit}", runtime::current_uid()),
        Linux => format!("systemctl --user stop {unit}"),
        Windows => format!("schtasks.exe /End /TN {unit}"),
    }
}
fn start_call(stack: &Stack, unloaded: bool) -> String {
    let unit = stack.platform.identifier();
    match stack.platform {
        Macos if unloaded => format!(
            "launchctl bootstrap gui/{} {}",
            runtime::current_uid(),
            stack.paths.definition.display()
        ),
        Macos => format!("launchctl kickstart gui/{}/{unit}", runtime::current_uid()),
        Linux => format!("systemctl --user start {unit}"),
        Windows => format!("schtasks.exe /Run /TN {unit}"),
    }
}

#[derive(Default)]
struct FakeHost {
    list: Option<SupervisorListResponse>,
    list_replies: VecDeque<SupervisorListResponse>,
    cached_health: Vec<SupervisorHealthEntry>,
    health_limits: Vec<Duration>,
    connect: bool,
    config: BTreeMap<String, bool>,
    rosters: VecDeque<Result<Option<LiveChildrenRoster>, RecordReadError>>,
    roster_path: Option<PathBuf>,
    processes: BTreeMap<u32, ProcessReading>,
    daemon_reads: VecDeque<ProcessReading>,
    daemon_epochs: VecDeque<u64>,
    image_reasons: BTreeMap<u32, SurvivorReason>,
    reads: Vec<&'static str>,
}
impl Host for FakeHost {
    fn list(&mut self, _: Duration) -> Option<SupervisorListResponse> {
        self.reads.push("list");
        self.list_replies.pop_front().or_else(|| self.list.clone())
    }
    fn cached_health(&mut self, limit: Duration) -> Vec<SupervisorHealthEntry> {
        self.health_limits.push(limit);
        self.cached_health.clone()
    }
    fn connects(&mut self, _: Duration) -> bool {
        self.reads.push("connect");
        self.connect
    }
    fn roster(&mut self) -> Result<Option<LiveChildrenRoster>, RecordReadError> {
        self.reads.push("roster");
        if let Some(next) = self.rosters.pop_front() {
            return next;
        }
        self.roster_path
            .as_deref()
            .map_or(Ok(None), subc_daemon::read_live_children_roster)
    }
    fn configured(&mut self) -> Result<BTreeMap<String, bool>, String> {
        Ok(self.config.clone())
    }
    fn process(&mut self, pid: u32) -> ProcessReading {
        self.reads.push("child identity");
        self.processes
            .get(&pid)
            .copied()
            .unwrap_or(ProcessReading::Gone)
    }
    fn daemon_process(&mut self, pid: u32) -> ProcessReading {
        self.reads.push("daemon identity");
        self.daemon_reads.pop_front().unwrap_or_else(|| {
            self.processes
                .get(&pid)
                .copied()
                .unwrap_or(ProcessReading::Gone)
        })
    }
    fn daemon_epoch(&mut self, pid: u32) -> Option<u64> {
        self.daemon_epochs.pop_front().or_else(|| {
            self.list
                .as_ref()?
                .daemon
                .as_ref()
                .filter(|row| row.pid == pid)
                .map(|row| row.start_time_ms)
        })
    }
    fn executable(&mut self, child: &LiveChild) -> Option<SurvivorReason> {
        self.image_reasons.get(&child.pid).copied()
    }
    fn same_boot(&mut self, _: &LiveChild) -> Option<bool> {
        Some(true)
    }
    fn container_empty(&mut self, _: &LiveChild) -> bool {
        false
    }
}

fn entry(name: &str, pid: u32) -> LiveChild {
    LiveChild {
        module_id: name.into(),
        pid,
        protocol: ModuleProtocol::Subc,
        start_time: Some(11),
        executable: Some(ExecutableIdentity::from(subc_os::FileIdentity {
            device: 1,
            inode: 2,
        })),
        cgroup_name: None,
        container: None,
    }
}
fn roster(children: Vec<LiveChild>) -> LiveChildrenRoster {
    LiveChildrenRoster {
        version: 2,
        children,
        retained_reasons: BTreeMap::new(),
    }
}
fn list(modules: Vec<SupervisorEntry>) -> SupervisorListResponse {
    SupervisorListResponse {
        generation: 0,
        modules,
        daemon: Some(subc_control::DaemonResources::new(42, 1234, "1.2.3")),
        agent_commands: None,
    }
}
fn wire(name: &str, state: &str, live: bool, health: SupervisorHealthStatus) -> SupervisorEntry {
    SupervisorEntry::new(name, state, true, live, health)
}
fn seed_roster(host: &mut FakeHost, path: PathBuf, children: Vec<LiveChild>) {
    fs::write(
        &path,
        serde_json::to_vec(&json!({"version":2,"children":children})).unwrap(),
    )
    .unwrap();
    host.roster_path = Some(path);
}

#[test]
fn stop_accepts_only_a_conclusive_already_stopped_reading() {
    for platform in PLATFORMS {
        for (seen, expected) in [
            (Seen::Idle, platform == Windows),
            (Seen::Absent, platform != Windows),
            (Seen::Live(None), false),
            (Seen::Failure, false),
            (Seen::Inconclusive, false),
        ] {
            let fixture = Fixture::new(platform);
            let clock = FakeClock::default();
            let mut runner = fixture.runner(&clock);
            runner.fail_stop = true;
            runner.default_job = seen;
            let result = fixture.lock("ck stack stop").stop_inner(
                Budget::new(&clock, SHORT),
                StopIntent::Stop,
                &mut FakeHost::default(),
                &mut runner,
            );
            assert_eq!(
                result.exit,
                if expected { 0 } else { 1 },
                "{platform:?}: {result:?}"
            );
            assert_eq!(runner.mutations(), [stop_call(&fixture.stack)]);
            assert_eq!(fixture.marker().exists(), expected);
            if expected {
                let marker = fs::read_to_string(fixture.marker()).unwrap();
                assert!(marker.starts_with("stopped_at_ms="));
                assert!(marker.ends_with("\ncommand=ck stack stop\n"));
            } else {
                assert!(result.manager_error.unwrap().contains("cannot stop"));
            }
        }
    }
}

#[test]
fn stop_waits_for_daemon_then_both_rosters_without_signalling() {
    for platform in PLATFORMS {
        let fixture = Fixture::new(platform);
        let clock = FakeClock::default();
        let mut runner = fixture.runner(&clock);
        let mut container = entry("container", 53);
        container.container = Some(Default::default());
        let mut host = FakeHost {
            list: Some(list(Vec::new())),
            rosters: VecDeque::from([
                Ok(Some(roster(vec![
                    entry("old", 50),
                    entry("reused", 51),
                    entry("hidden", 52),
                    container,
                ]))),
                Ok(Some(roster(vec![entry("fresh", 54)]))),
            ]),
            processes: BTreeMap::from([
                (42, ProcessReading::Alive(1234)),
                (50, ProcessReading::Alive(11)),
                (51, ProcessReading::Alive(12)),
                (52, ProcessReading::Unreadable),
                (54, ProcessReading::Alive(11)),
            ]),
            image_reasons: BTreeMap::from([(54, SurvivorReason::ExecutableChanged)]),
            ..FakeHost::default()
        };
        let timeout = Duration::from_secs(61);
        let result = fixture.lock("ck stack stop").stop_inner(
            Budget::new(&clock, timeout),
            StopIntent::Stop,
            &mut host,
            &mut runner,
        );
        assert_eq!(result.exit, 1);
        assert!(result.daemon_past_bound);
        assert_eq!(clock.elapsed(), timeout);
        assert_eq!(runner.mutations(), [stop_call(&fixture.stack)]);
        assert!(!runner.calls.iter().any(|call| call.contains("intent")));
        assert_eq!(&host.reads[..3], ["list", "roster", "daemon identity"]);
        let fresh = host
            .reads
            .iter()
            .rposition(|read| *read == "roster")
            .unwrap();
        assert_eq!(host.reads[fresh - 1], "daemon identity");
        assert_eq!(host.reads[fresh + 1], "child identity");
        assert_eq!(
            result
                .survivors
                .iter()
                .map(|child| (child.name.as_str(), child.reason))
                .collect::<Vec<_>>(),
            [
                ("old", None),
                ("hidden", Some(SurvivorReason::IdentityUnreadable)),
                ("container", Some(SurvivorReason::ContainerUnproven)),
                ("fresh", Some(SurvivorReason::ExecutableChanged)),
            ]
        );
        assert!(result.human(true).contains("old (pid 50, executable"));
        assert!(fixture.marker().exists());
        let json = serde_json::to_value(result).unwrap();
        let survivor = json["survivors"][0].as_object().unwrap();
        let keys = ["executable", "name", "pid", "reason", "start_time", "state"];
        assert_eq!(survivor.len(), keys.len());
        assert!(keys.iter().all(|key| survivor.contains_key(*key)));
    }
}

#[test]
fn stop_does_not_count_a_daemon_gone_after_its_epoch_moves() {
    let fixture = Fixture::new(RuntimePlatform::Linux);
    let clock = FakeClock::default();
    let mut runner = fixture.runner(&clock);
    let mut host = FakeHost {
        list: Some(list(Vec::new())),
        processes: BTreeMap::from([(42, ProcessReading::Alive(123))]),
        // Raw start ticks are fixed; the projected boot time advances by two seconds.
        daemon_epochs: VecDeque::from([1234, 3234]),
        ..FakeHost::default()
    };
    let result = fixture.lock("ck stack stop").stop_inner(
        Budget::new(&clock, Duration::from_secs(61)),
        StopIntent::Stop,
        &mut host,
        &mut runner,
    );
    assert!(result.daemon_past_bound);
    assert_eq!(result.exit, 1);
    assert_eq!(host.daemon_epoch(42), Some(3234));
}

#[test]
fn stop_reports_roster_errors_but_still_marks_a_stopped_manager() {
    for platform in PLATFORMS {
        for (data, expected) in [
            (Some("{"), Some(RosterErrorKind::Malformed)),
            (
                Some(r#"{"version":9,"children":[]}"#),
                Some(RosterErrorKind::UnknownVersion),
            ),
            (Some(r#"{"version":1,"children":[]}"#), None),
            (Some(r#"{"version":2,"children":[]}"#), None),
            (None, None),
        ] {
            let fixture = Fixture::new(platform);
            let path = fixture._tmp.join("live-children.json");
            if let Some(data) = data {
                fs::write(&path, data).unwrap();
            }
            let clock = FakeClock::default();
            let mut runner = fixture.runner(&clock);
            let mut host = FakeHost {
                roster_path: Some(path),
                ..FakeHost::default()
            };
            let result = fixture.lock("ck stack stop").stop_inner(
                Budget::new(&clock, SHORT),
                StopIntent::Stop,
                &mut host,
                &mut runner,
            );
            assert_eq!(result.exit, u8::from(expected.is_some()));
            assert_eq!(result.roster_error.map(|error| error.kind), expected);
            assert!(fixture.marker().exists());
        }
        // An I/O error only in the fresh read must also fail the stop.
        let fixture = Fixture::new(platform);
        let path = fixture._tmp.join("live-children.json");
        fs::create_dir(&path).unwrap();
        let clock = FakeClock::default();
        let mut runner = fixture.runner(&clock);
        let mut host = FakeHost {
            roster_path: Some(path),
            rosters: VecDeque::from([Ok(None)]),
            ..FakeHost::default()
        };
        let result = fixture.lock("ck stack stop").stop_inner(
            Budget::new(&clock, SHORT),
            StopIntent::Stop,
            &mut host,
            &mut runner,
        );
        assert_eq!(result.exit, 1);
        assert_eq!(
            result.roster_error.unwrap().kind,
            RosterErrorKind::Unreadable
        );
        assert!(fixture.marker().exists());
    }
}

#[test]
fn stop_checks_daemon_fallback_final_manager_and_pid_reuse() {
    for platform in PLATFORMS {
        for (final_read, connects, daemon_reading, expected, past_bound) in [
            (Seen::Idle, false, None, 0, false),
            (Seen::Idle, true, None, 1, true),
            (Seen::Live(None), false, None, 1, true),
            (
                Seen::Live(Some(88)),
                false,
                Some(ProcessReading::Gone),
                1,
                false,
            ),
            (
                Seen::Failure,
                false,
                Some(ProcessReading::Alive(1235)),
                1,
                false,
            ),
        ] {
            let fixture = Fixture::new(platform);
            let clock = FakeClock::default();
            let mut runner = fixture.runner(&clock);
            runner.default_job = final_read;
            let mut host = FakeHost {
                list: daemon_reading.map(|_| list(Vec::new())),
                connect: connects,
                processes: BTreeMap::from([(55, ProcessReading::Alive(12))]),
                rosters: VecDeque::from([Ok(Some(roster(vec![entry("reused", 55)])))]),
                ..FakeHost::default()
            };
            if let Some(reading) = daemon_reading {
                // Bind the listed birth before the manager stop, then inspect
                // the PID again after it has departed or been reused.
                host.daemon_reads = VecDeque::from([ProcessReading::Alive(100); 2]);
                host.processes.insert(42, reading);
            }
            let result = fixture.lock("ck stack stop").stop_inner(
                Budget::new(&clock, SHORT),
                StopIntent::Stop,
                &mut host,
                &mut runner,
            );
            assert_eq!(result.exit, expected, "{platform:?} {result:?}");
            assert_eq!(result.daemon_past_bound, past_bound);
            assert!(result.survivors.is_empty());
            assert!(fixture.marker().exists());
            assert_eq!(
                result.manager_error.is_some(),
                matches!(final_read, Seen::Failure)
            );
            if platform == Macos && matches!(final_read, Seen::Live(Some(88))) {
                assert_eq!(result.relaunched_pid, Some(88));
            }
        }
    }
}

#[test]
fn only_revert_restart_intent_is_announced_before_the_manager_stop() {
    for platform in PLATFORMS {
        let fixture = Fixture::new(platform);
        let clock = FakeClock::default();
        let mut runner = fixture.runner(&clock);
        let mut host = FakeHost {
            list: Some(list(Vec::new())),
            ..FakeHost::default()
        };
        let result = fixture.lock("ck window revert").stop_inner(
            Budget::new(&clock, Duration::from_secs(3)),
            StopIntent::Restart,
            &mut host,
            &mut runner,
        );
        assert_eq!(result.exit, 0);
        assert_eq!(runner.calls[0], "shutdown intent restart");
        assert_eq!(runner.mutations(), [stop_call(&fixture.stack)]);
    }
}

#[test]
fn start_refusal_order_protects_the_marker_until_the_job_is_stale() {
    for platform in PLATFORMS {
        for (seen, window, connects, removed, evidence) in [
            (
                Seen::Failure,
                WindowCheck::Reverting {
                    id: "blocked".into(),
                },
                true,
                false,
                "Service manager:",
            ),
            (
                Seen::Live(None),
                WindowCheck::Reverting {
                    id: "in-progress".into(),
                },
                true,
                false,
                "window in-progress is reverting",
            ),
            (
                Seen::Live(None),
                WindowCheck::RevertInterrupted { id: "held".into() },
                true,
                false,
                "window held has an interrupted revert",
            ),
            (
                Seen::Live(None),
                WindowCheck::Unreadable {
                    id: "bad".into(),
                    error: "torn".into(),
                },
                true,
                false,
                "window bad is unreadable: torn",
            ),
            (
                Seen::Live(None),
                WindowCheck::Open,
                false,
                true,
                "reports the daemon live",
            ),
            (
                Seen::Live(Some(99)),
                WindowCheck::NoWindow,
                false,
                true,
                "reports the daemon live",
            ),
            (
                Seen::Idle,
                WindowCheck::NoWindow,
                true,
                false,
                "already answering connections",
            ),
        ] {
            let fixture = Fixture::new(platform);
            fs::write(fixture.marker(), "unchanged").unwrap();
            let clock = FakeClock::default();
            let mut runner = fixture.runner(&clock);
            runner.default_job = seen;
            let checked = Cell::new(false);
            let mut host = FakeHost {
                connect: connects,
                ..FakeHost::default()
            };
            let result = fixture.lock("ck stack start").start_inner(
                Budget::new(&clock, SHORT),
                || {
                    checked.set(true);
                    window
                },
                &mut host,
                &mut runner,
            );
            assert_eq!(result.exit, 2);
            assert_eq!(checked.get(), !matches!(seen, Seen::Failure));
            assert_eq!(
                host.reads,
                if matches!(seen, Seen::Idle) {
                    vec!["connect"]
                } else {
                    vec![]
                }
            );
            assert!(result.human(false).contains(evidence));
            assert_eq!(result.marker_removed, removed);
            assert_eq!(fixture.marker().exists(), !removed);
            assert!(runner.mutations().is_empty());
        }
    }
}

#[test]
fn start_deadline_distinguishes_manager_failure_unreachable_and_empty() {
    for platform in PLATFORMS {
        for (fail_start, answers, exit) in [(true, false, 1), (false, false, 1), (false, true, 0)] {
            let fixture = Fixture::new(platform);
            fs::write(fixture.marker(), "stop").unwrap();
            let clock = FakeClock::default();
            let mut runner = fixture.runner(&clock);
            runner.default_job = if platform == Macos {
                Seen::Absent
            } else {
                Seen::Idle
            };
            runner.fail_start = fail_start;
            let mut host = FakeHost {
                list: answers.then(|| list(vec![])),
                ..FakeHost::default()
            };
            let result = fixture.lock("ck stack start").start_inner(
                Budget::new(&clock, SHORT),
                || WindowCheck::NoWindow,
                &mut host,
                &mut runner,
            );
            assert_eq!(runner.mutations(), [start_call(&fixture.stack, true)]);
            assert_eq!(fixture.marker().exists(), fail_start);
            assert_eq!(result.exit, exit);
            assert_eq!(result.daemon_unreachable, !answers && !fail_start);
            assert_eq!(
                clock.elapsed(),
                if result.daemon_unreachable {
                    SHORT
                } else {
                    Duration::ZERO
                }
            );
            assert_eq!(result.manager_error.is_some(), fail_start);
            if fail_start {
                assert!(result
                    .manager_error
                    .as_ref()
                    .unwrap()
                    .contains("cannot start"));
            }
            let json = serde_json::to_value(result).unwrap();
            assert_eq!(json["schema"], "ck.stack.result/v1");
            assert_eq!(json["exit"], exit);
            for key in [
                "survivors",
                "unhealthy",
                "not_health_checked",
                "degraded",
                "restored",
                "marker_removed",
                "daemon_unreachable",
                "daemon_past_bound",
                "relaunched_pid",
                "manager_error",
                "roster_error",
            ] {
                assert!(json.get(key).is_some(), "missing result key {key}");
            }
        }
    }
}

#[test]
fn start_classifies_health_and_expected_modules_at_the_injected_deadline() {
    use SupervisorHealthStatus::{Failing, Ok as Healthy, Unknown};
    for platform in PLATFORMS {
        for scenario in 0..3 {
            let fixture = Fixture::new(platform);
            let clock = FakeClock::default();
            let mut runner = fixture.runner(&clock);
            let mut modules = vec![
                wire("approved", "Running", true, Healthy)
                    .with_configured_enabled(Some(true))
                    .with_last_probe_ms(Some(1)),
                wire("not-approved", "Failed", false, Failing).with_configured_enabled(Some(false)),
                wire("process-only", "Running", true, Unknown).with_protocol(ModuleProtocol::None),
            ];
            modules[0].enabled = false;
            let mut children = vec![entry("approved", 50), entry("process-only", 51)];
            if scenario > 0 {
                modules.push(wire("not-probed", "Running", false, Unknown));
                children.push(entry("not-probed", 52));
            }
            if scenario > 1 {
                modules.extend([
                    wire("probe-failed", "Running", true, Failing).with_last_probe_ms(Some(1)),
                    wire("probe-unknown", "Running", true, Unknown).with_last_probe_ms(Some(1)),
                    wire("http", "Running", true, Unknown).with_protocol(ModuleProtocol::None),
                    wire("down", "Restarting", false, Healthy),
                    wire("no-roster-pid", "Running", true, Healthy).with_last_probe_ms(Some(1)),
                ]);
                children.extend([
                    entry("probe-failed", 53),
                    entry("probe-unknown", 54),
                    entry("http", 55),
                ]);
            }
            let mut host = FakeHost {
                list: Some(list(modules)),
                config: BTreeMap::from([
                    ("not-approved".into(), true),
                    ("process-only".into(), false),
                    ("http".into(), true),
                ]),
                processes: children
                    .iter()
                    .map(|child| (child.pid, ProcessReading::Alive(11)))
                    .collect(),
                ..FakeHost::default()
            };
            seed_roster(&mut host, fixture._tmp.join("live-children.json"), children);
            let result = fixture.lock("ck stack start").start_inner(
                Budget::new(&clock, SHORT),
                || WindowCheck::NoWindow,
                &mut host,
                &mut runner,
            );
            assert_eq!(
                clock.elapsed(),
                if scenario == 0 { Duration::ZERO } else { SHORT }
            );
            assert_eq!(result.exit, u8::from(scenario > 1));
            assert!(host.health_limits.is_empty());
            let human = result.human(false);
            let json = serde_json::to_value(&result).unwrap();
            if scenario > 0 {
                assert!(human.contains("Running, not health-checked: not-probed"));
                assert_eq!(json["not_health_checked"], json!(["not-probed"]));
            }
            if scenario > 1 {
                assert!(human
                    .contains("Unhealthy: probe-failed, probe-unknown, http, down, no-roster-pid"));
                assert!(
                    human.contains("supervisor.list reports serving; roster reports no live entry")
                );
                assert_eq!(
                    json["unhealthy"],
                    json!([
                        "probe-failed",
                        "probe-unknown",
                        "http",
                        "down",
                        "no-roster-pid"
                    ])
                );
            }
        }
    }
}

#[test]
fn start_lists_degraded_at_deadline_without_failing() {
    use SupervisorHealthStatus::{Degraded, Ok as Healthy};
    let note = "7 sessions waiting for the previous lease to expire";
    for platform in PLATFORMS {
        for cached_status in [Some(Degraded), Some(Healthy), None] {
            let fixture = Fixture::new(platform);
            let clock = FakeClock::default();
            let mut runner = fixture.runner(&clock);
            let module = wire("index", "Running", true, Degraded).with_last_probe_ms(Some(1));
            let mut host = FakeHost {
                list: Some(list(vec![module])),
                cached_health: cached_status.into_iter().map(|status| {
                    serde_json::from_value(json!({
                        "module_id": "index", "status": status, "detail": note, "consecutive_failures": 0,
                    })).unwrap()
                }).collect(),
                processes: BTreeMap::from([(50, ProcessReading::Alive(11))]),
                ..FakeHost::default()
            };
            seed_roster(
                &mut host,
                fixture._tmp.join("live-children.json"),
                vec![entry("index", 50)],
            );
            let result = fixture.lock("ck stack start").start_inner(
                Budget::new(&clock, SHORT),
                || WindowCheck::NoWindow,
                &mut host,
                &mut runner,
            );
            assert_eq!(clock.elapsed(), SHORT);
            assert_eq!(
                result.exit, 0,
                "a running degraded module must not fail start"
            );
            assert!(result.unhealthy.is_empty());
            assert_eq!(host.health_limits, [SHORT]);
            let detail = (cached_status == Some(Degraded)).then_some(note);
            let human = result.human(false);
            assert!(human.contains("index: running, degraded"));
            assert_eq!(
                human.contains(&format!("running, degraded: {note}")),
                detail.is_some()
            );
            assert_eq!(
                serde_json::to_value(result).unwrap()["degraded"],
                json!([{"name": "index", "detail": detail}])
            );
        }
    }
}

#[test]
fn start_returns_early_when_degraded_module_recovers() {
    for platform in PLATFORMS {
        let fixture = Fixture::new(platform);
        let clock = FakeClock::default();
        let mut runner = fixture.runner(&clock);
        let degraded = wire("index", "Running", true, SupervisorHealthStatus::Degraded)
            .with_last_probe_ms(Some(1));
        let healthy =
            wire("index", "Running", true, SupervisorHealthStatus::Ok).with_last_probe_ms(Some(2));
        let mut host = FakeHost {
            list_replies: VecDeque::from([list(vec![degraded]), list(vec![healthy])]),
            processes: BTreeMap::from([(50, ProcessReading::Alive(11))]),
            ..FakeHost::default()
        };
        seed_roster(
            &mut host,
            fixture._tmp.join("live-children.json"),
            vec![entry("index", 50)],
        );
        let result = fixture.lock("ck stack start").start_inner(
            Budget::new(&clock, SHORT),
            || WindowCheck::NoWindow,
            &mut host,
            &mut runner,
        );
        assert_eq!(clock.elapsed(), POLL);
        assert_eq!(result.exit, 0);
        assert_eq!(host.health_limits, [SHORT]);
        assert!(!result.human(false).contains("degraded"));
        assert_eq!(serde_json::to_value(result).unwrap()["degraded"], json!([]));
    }
}

#[test]
fn doctor_classifies_unanswered_daemon_from_manager_and_marker() {
    for (platform, seen, marker, manager, pid, service, daemon, manager_error) in [
        (
            Macos,
            Seen::Live(Some(42)),
            "stale",
            "running",
            Some(42),
            Some(("service.stale_marker", Severity::Info)),
            "daemon.unreachable",
            None,
        ),
        (
            Linux,
            Seen::Live(None),
            "absent",
            "running",
            None,
            None,
            "daemon.unreachable",
            None,
        ),
        (
            Linux,
            Seen::Idle,
            "absent",
            "loaded",
            None,
            Some(("service.not_running", Severity::Error)),
            "daemon.not_running",
            None,
        ),
        (
            Macos,
            Seen::Absent,
            "absent",
            "not_loaded",
            None,
            Some(("service.not_loaded", Severity::Error)),
            "daemon.not_running",
            None,
        ),
        (
            Windows,
            Seen::Failure,
            "absent",
            "unknown",
            None,
            Some(("service.unknown", Severity::Warning)),
            "daemon.unreachable",
            Some("Access is denied"),
        ),
        (
            Windows,
            Seen::Inconclusive,
            "unknown",
            "unknown",
            None,
            Some(("service.unknown", Severity::Warning)),
            "daemon.unreachable",
            Some("did not report a conclusive status"),
        ),
    ] {
        let fixture = Fixture::new(platform);
        if marker != "absent" {
            fs::write(fixture.marker(), "stop").unwrap();
        }
        let clock = FakeClock::default();
        let mut runner = fixture.runner(&clock);
        runner.jobs.push_back(seen);
        let (job, marker_state) =
            observe_manager_and_marker(platform, &mut runner, &fixture.stack.root);
        assert!(runner.jobs.is_empty());
        assert!(runner.mutations().is_empty());

        let findings =
            classify_service_and_daemon(job, marker_state, &DescribeOutcome::default(), None);
        let expected = service.into_iter().chain([(daemon, Severity::Error)]);
        assert!(
            findings
                .iter()
                .map(|f| (f.id.as_str(), f.severity))
                .eq(expected),
            "{platform:?} {manager} {marker}: {findings:?}"
        );
        if service.is_some() {
            assert_eq!(findings[0].evidence["marker"], marker);
        }
        let evidence = &findings.last().unwrap().evidence;
        assert_eq!(evidence["manager"], manager);
        assert_eq!(evidence["recorded_pid"], json!(pid));
        match manager_error {
            Some(reason) => assert!(evidence["manager_error"].as_str().unwrap().contains(reason)),
            None => assert_eq!(evidence["manager_error"], Value::Null),
        }
    }
}

#[test]
fn doctor_describe_deadline_keeps_marker_based_deliberate_stop_verdict() {
    let fixture = Fixture::new(Windows);
    fs::write(fixture.marker(), "stop").unwrap();
    let clock = FakeClock::default();
    let mut runner = fixture.runner(&clock);
    runner.jobs.push_back(Seen::Idle);
    let observation = observe_manager_and_marker(Windows, &mut runner, &fixture.stack.root);
    assert!(runner.jobs.is_empty());
    assert!(runner.mutations().is_empty());

    let start = Instant::now();
    let result = run_with_blocked_describe(observation);
    assert!(start.elapsed() < Duration::from_secs(15));
    assert_eq!(result.exit_code, 1);
    assert!(!result.document.daemon_answered);
    let findings = &result.document.findings;
    let stopped = findings.iter().find(|f| f.id == "service.stopped").unwrap();
    assert_eq!(stopped.severity, Severity::Warning);
    assert_eq!(stopped.evidence["marker"], "present");
    assert_eq!(stopped.evidence["manager"], "loaded");
    let daemon = findings
        .iter()
        .find(|f| f.id == "daemon.not_running")
        .unwrap();
    assert_eq!(daemon.evidence["connection_pid_hint"], std::process::id());
    let errors: Vec<_> = findings
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .map(|f| f.id.as_str())
        .collect();
    assert_eq!(errors, ["daemon.not_running"]);
    assert!(!findings.iter().any(|f| f.id == "daemon.timed_out"));
}

#[test]
fn status_renders_offline_roster_errors_and_live_source_disagreement() {
    for platform in PLATFORMS {
        let fixture = Fixture::new(platform);
        fs::write(fixture.marker(), "stop").unwrap();
        let clock = FakeClock::default();
        let mut runner = fixture.runner(&clock);
        runner.default_job = Seen::Failure;
        let roster_path = fixture._tmp.join("live-children.json");
        fs::write(&roster_path, "bad roster").unwrap();
        let mut host = FakeHost {
            config: BTreeMap::from([("configured".into(), false)]),
            roster_path: Some(roster_path.clone()),
            ..FakeHost::default()
        };
        let report = fixture
            .stack
            .status_inner(&clock, SHORT, &mut host, &mut runner);
        assert_eq!(clock.elapsed(), SHORT);
        assert_eq!(report.source, Source::ManagerAndRoster);
        assert_eq!(report.manager, ManagerState::Unknown);
        assert_eq!(report.stopped_marker, MarkerState::Unknown);
        assert_eq!(report.daemon_version, "unknown");
        assert_eq!(report.modules[0].state, ModuleState::Indeterminate);
        assert_eq!(report.modules[0].health, Health::Unknown);
        assert_eq!(
            report.roster_error.as_ref().unwrap().kind,
            RosterErrorKind::Malformed
        );
        let json = serde_json::to_value(&report).unwrap();
        for key in [
            "source",
            "manager",
            "stopped_marker",
            "daemon_pid",
            "daemon_start_time",
            "daemon_version",
            "roster_error",
            "modules",
        ] {
            assert!(json.get(key).is_some(), "missing status key {key}");
        }
        assert_eq!(json["schema"], "ck.stack.status/v1");
        assert_eq!(json["source"], "manager+roster");
        assert_eq!(json["daemon_start_time"], Value::Null);
        assert!(report.human().contains("Roster malformed:"));
        assert!(runner.mutations().is_empty());

        runner.default_job = Seen::Live(Some(42));
        let mut down = wire(
            "disagreement",
            "Failed",
            false,
            SupervisorHealthStatus::Failing,
        );
        down.tree_unavailable = Some("not_running".into());
        host.list = Some(list(vec![
            wire(
                "stopped",
                "Restarting",
                false,
                SupervisorHealthStatus::Unknown,
            ),
            down,
        ]));
        host.processes.insert(55, ProcessReading::Alive(11));
        seed_roster(&mut host, roster_path, vec![entry("disagreement", 55)]);
        host.reads.clear();
        let report = fixture
            .stack
            .status_inner(&clock, SHORT, &mut host, &mut runner);
        assert_eq!(&host.reads[..2], ["list", "roster"]);
        assert_eq!(report.source, Source::Daemon);
        assert_eq!(report.stopped_marker, MarkerState::Stale);
        assert_eq!(report.daemon_start_time, Some(1234));
        assert_eq!(report.daemon_version, "1.2.3");
        assert_eq!(report.modules[0].state, ModuleState::Stopped);
        assert_eq!(report.modules[1].state, ModuleState::Indeterminate);
        assert!(report
            .human()
            .contains("supervisor.list reports not running; roster reports running"));
    }
}

#[test]
fn presence_precedes_locks_and_binding_and_refusals_leave_the_marker() {
    for platform in PLATFORMS {
        let fixture = Fixture::new(platform);
        fs::write(fixture.marker(), "marker").unwrap();
        let holder = fixture.lock("ck window revert");
        for (prompt, command) in [
            (STOP_PROMPT, "ck stack stop"),
            (START_PROMPT, "ck stack start"),
        ] {
            for outcome in [
                Outcome::Declined,
                Outcome::AuthorizedWithoutPrompt,
                Outcome::UnsupportedPlatform,
                Outcome::SetupRequired,
                Outcome::Approved,
            ] {
                let mut presence = FakePresence {
                    texts: Vec::new(),
                    outcome: outcome.clone(),
                };
                let error = match fixture
                    .stack
                    .confirm_and_lock(prompt, command, &mut presence)
                {
                    Ok(_) => panic!("a second command acquired the revert's locks"),
                    Err(error) => error,
                };
                assert_eq!(presence.texts, [prompt]);
                if outcome == Outcome::Approved {
                    assert!(error.contains("ck window revert"), "{error}");
                } else {
                    assert!(error.contains(&format!("{outcome:?}")), "{error}");
                }
                assert_eq!(fs::read_to_string(fixture.marker()).unwrap(), "marker");
            }
        }
        drop(holder);
        let second_root = fixture._tmp.join("other-install");
        fs::create_dir(&second_root).unwrap();
        let definition = fixture.stack.paths.definition.clone();
        let mut paths =
            runtime::runtime_paths(platform, &second_root.join("bin"), fixture._tmp.path());
        paths.definition = definition.clone();
        let wrong = Stack {
            root: second_root.clone(),
            paths,
            platform,
            config: PathBuf::new(),
            connection: None,
        };
        let mut presence = FakePresence {
            texts: Vec::new(),
            outcome: Outcome::Approved,
        };
        for (prompt, command) in [
            (STOP_PROMPT, "ck stack stop"),
            (START_PROMPT, "ck stack start"),
        ] {
            let error = match wrong.confirm_and_lock(prompt, command, &mut presence) {
                Ok(_) => panic!("wrong install bound to the job"),
                Err(error) => error,
            };
            assert!(error.contains(&definition.display().to_string()));
            assert!(error.contains(&second_root.display().to_string()));
        }
        assert_eq!(presence.texts, [STOP_PROMPT, START_PROMPT]);
        assert_eq!(fs::read_to_string(fixture.marker()).unwrap(), "marker");
    }
    assert_eq!(STOP_PROMPT, "Allow `ck stack stop` to stop the stack? Modules will be told to stop and give up long-running work.");
    assert_eq!(START_PROMPT, "Allow `ck stack start` to start the stack?");
}

#[test]
fn manager_completion_past_the_deadline_fails_without_cancelling_and_updates_marker() {
    for (platform, verb) in [(Macos, "bootout"), (Linux, "stop"), (Windows, "/End")] {
        let fixture = Fixture::new(platform);
        let clock = FakeClock::default();
        let mut runner = fixture.runner(&clock);
        runner.slow = Some((verb, Duration::from_secs(1)));
        let mut host = FakeHost {
            list: Some(list(Vec::new())),
            ..FakeHost::default()
        };
        let result = fixture.lock("ck stack stop").stop_inner(
            Budget::new(&clock, SHORT),
            StopIntent::Stop,
            &mut host,
            &mut runner,
        );
        assert_eq!(result.exit, 1);
        assert_eq!(runner.mutations(), [stop_call(&fixture.stack)]);
        assert!(fixture.marker().exists());
        assert!(result.human(true).contains(&format!(
            "`{}` took longer than the deadline",
            stop_call(&fixture.stack)
        )));
        assert!(serde_json::to_value(result).unwrap()["message"]
            .as_str()
            .unwrap()
            .contains("took longer than the deadline"));
    }
    let fixture = Fixture::new(Linux);
    let clock = FakeClock::default();
    let mut runner = fixture.runner(&clock);
    runner.slow = Some(("show", Duration::from_secs(1)));
    let result = fixture.lock("ck stack stop").stop_inner(
        Budget::new(&clock, SHORT),
        StopIntent::Stop,
        &mut FakeHost::default(),
        &mut runner,
    );
    assert_eq!(result.exit, 1);
    assert!(runner.mutations().is_empty());
    assert!(!fixture.marker().exists());
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn replaced_executable_is_not_proof_that_a_live_process_is_gone() {
    use std::{io::Read as _, process::Stdio};

    let fixture = Fixture::new(RuntimePlatform::current());
    let original = fixture._tmp.join("waiter");
    let replacement = fixture._tmp.join("replacement");
    fs::copy("/bin/bash", &original).unwrap();
    fs::copy("/bin/bash", &replacement).unwrap();
    // Use an ad-hoc signature because the copy is no longer a system binary.
    #[cfg(target_os = "macos")]
    assert!(std::process::Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", "-"])
        .arg(&original)
        .status()
        .unwrap()
        .success());
    let mut child = std::process::Command::new(&original)
        .args([
            "--noprofile",
            "--norc",
            "-c",
            "printf ready; read -r line || :",
        ])
        .env_remove("BASH_ENV")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = [0; 5];
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut ready)
        .unwrap();
    assert_eq!(&ready, b"ready");
    let mut record = entry("waiter", child.id());
    record.start_time = subc_os::start_time(child.id());
    assert!(record.start_time.is_some());
    record.executable = subc_os::file_identity(&original).map(ExecutableIdentity::from);
    let mut host = fixture.stack.engine().unwrap();
    assert_eq!(survivor_reason(&mut host, &record, false), Some(None));
    fs::rename(replacement, original).unwrap();
    let reason = survivor_reason(&mut host, &record, false);
    assert!(matches!(
        reason,
        Some(Some(
            SurvivorReason::ExecutableChanged | SurvivorReason::IdentityUnreadable
        ))
    ));
    #[cfg(target_os = "linux")]
    assert_eq!(reason, Some(Some(SurvivorReason::ExecutableChanged)));
    // Closing the pipe lets the running child exit without a signal.
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
    assert_eq!(survivor_reason(&mut host, &record, false), None);
}
