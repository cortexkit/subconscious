use super::*;
use cortexkit_test_support::ScratchDir;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// The stub, expected beside this test executable.
///
/// The existence check is here for the reason its twin at `fake_aft_stub_path`
/// documents: `--lib` does not build `[[bin]]` targets, and a bare spawn
/// failure then reads as a broken test rather than an unbuilt dependency.
fn stub_path() -> PathBuf {
    let path = cortexkit_test_support::cargo_profile_bin("fake-aft-stub");
    assert!(
        path.exists(),
        "fake-aft-stub not built at {}: run `cargo test -p subc-core` (which builds \
             [[bin]] targets) rather than `cargo test -p subc-core --lib` (which does not)",
        path.display()
    );
    path
}

/// Poll for the grandchild pid the stub records, and parse it.
fn read_grandchild_pid(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(contents) = std::fs::read_to_string(path) {
            if let Ok(pid) = contents.trim().parse() {
                return pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the stub never recorded a grandchild pid at {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Everything one fixture run needs, so the two tests below differ in exactly
/// one place: whether the child is contained.
struct Fixture {
    _dir: ScratchDir,
    _roster: ChildRoster,
    module_id: String,
    grandchild: u32,
    child: Option<SupervisedChild>,
    registry: Arc<Registry>,
    snapshot: Arc<Mutex<SupervisorSnapshot>>,
    terminal_ring: Arc<Mutex<TerminalRing>>,
    spawn_events: SpawnEventFeed,
}

fn fixture(label: &str, module_id: &str) -> Fixture {
    fixture_with_carry(label, module_id, false)
}

fn fixture_with_carry(label: &str, module_id: &str, carry_containment: bool) -> Fixture {
    let dir = ScratchDir::new(label);
    let pid_file = dir.join("grandchild.pid");
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(3, Duration::ZERO),
    );
    let runtime = supervisor.runtime_config();
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    let spec = ModuleSpec {
        script: None,
        module_id: module_id.to_string(),
        program: stub_path(),
        // Zero args deliberately: a `--subc` argument would make the stub dial
        // a daemon that is not there, and the failure would land in the same
        // stderr ring this fixture exists to keep quiet.
        args: Vec::new(),
        env: vec![
            ("FAKE_AFT_NEVER_CONNECT".to_string(), "1".to_string()),
            (
                "FAKE_AFT_GRANDCHILD_PID_FILE".to_string(),
                pid_file.display().to_string(),
            ),
        ],
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment,
    };
    let child =
        spawn_and_mark_running(&spec, &runtime, &snapshot).expect("spawn the supervised fixture");
    let grandchild = read_grandchild_pid(&pid_file);
    Fixture {
        _dir: dir,
        _roster: runtime.child_roster.clone(),
        module_id: module_id.to_string(),
        grandchild,
        child: Some(child),
        registry: Arc::new(Registry::default()),
        snapshot,
        terminal_ring: Arc::clone(&runtime.terminal_ring),
        spawn_events: SpawnEventFeed::default(),
    }
}

async fn assert_admission_kill_reaps_grandchild(carry: bool) {
    let label = if carry {
        "admission-kill-carried-tree"
    } else {
        "admission-kill-plain-tree"
    };
    let mut fixture = fixture_with_carry(label, label, carry);
    let descendant = subc_os::Process::open(fixture.grandchild)
        .unwrap()
        .expect("the stub descendant must be alive before admission kill");
    assert!(descendant.observe().is_some());
    let child = fixture.child.as_mut().unwrap();
    assert_eq!(child.child.container().is_some(), carry);
    assert_eq!(child.job.is_some(), !carry);

    child.start_admission_kill().unwrap();
    assert!(child.admission_kill_sent);
    let status = timeout(Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        status.code(),
        Some(admission_exit::WINDOWS_ADMISSION_KILL_EXIT_CODE),
        "containment teardown must preserve the leader's distinctive status"
    );
    // Keep the child and its job/container alive: later exit finalization or
    // handle drop must not be what makes the descendant-death claim pass.
    assert!(
        descendant.wait_for_exit(Duration::from_secs(10)).unwrap(),
        "admission kill left a descendant running before exit finalization"
    );
    assert_eq!(child.child.container().is_some(), carry);
    assert_eq!(child.job.is_some(), !carry);
    child.finish_exit(false).await.unwrap();
}

#[tokio::test]
async fn admission_kill_reaps_grandchild_with_plain_job() {
    assert_admission_kill_reaps_grandchild(false).await;
}

#[tokio::test]
async fn admission_kill_reaps_grandchild_with_carried_job() {
    assert_admission_kill_reaps_grandchild(true).await;
}

#[tokio::test]
async fn admission_kill_retires_registration_before_wait() {
    let mut fixture = fixture("admission-registration", "admission-registration");
    let handle = SupervisorHandle::new();
    let spawn = RegistrationSpawn::new(&handle, "admission-registration", SpawnRole::Plain, None);
    spawn.ready();
    let child = fixture.child.as_mut().unwrap();
    child.registration_spawn = Some(spawn.clone());
    child.start_admission_kill().unwrap();
    assert!(
        !spawn.launch_ready(),
        "admission kill must retire before its caller can await the child"
    );
    timeout(Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap();
    child.finish_exit(false).await.unwrap();
}

impl Fixture {
    /// Drain through the supervisor's own teardown path.
    async fn drain(&mut self) {
        let child = self
            .child
            .take()
            .expect("the fixture child is still present");
        drain_child_to_state(
            &self.module_id,
            ModuleProtocol::Subc,
            // No forwarding table in this fixture, so nothing reaches the
            // child over a connection.
            StopNotice::NotSent,
            &self.registry,
            None,
            &self.snapshot,
            &self.terminal_ring,
            &self.spawn_events,
            child,
            Duration::from_millis(500),
            ModuleState::Stopped,
            Some(false),
            &FailureContext::operator(),
        )
        .await
        .expect("drain the supervised fixture");
    }
}

/// Teardown reaps the grandchild, not merely the direct child.
///
/// This is the assertion the change exists for. Before containment the
/// grandchild survived: it is a separate process, and `start_kill` is
/// `TerminateProcess` scoped to one pid.
#[tokio::test]
async fn teardown_reaps_the_grandchild() {
    let mut fixture = fixture("teardown-grandchild", "tree-teardown");
    let grandchild = fixture.grandchild;

    assert!(
        subc_jobobject::process_exists(grandchild),
        "grandchild {grandchild} must be alive before teardown, or this proves nothing"
    );

    fixture.drain().await;

    assert!(
        subc_jobobject::wait_for_process_exit(grandchild, Duration::from_secs(10)),
        "grandchild {grandchild} outlived module teardown: the tree was not contained"
    );
}

/// The mutation control: with containment withheld, the grandchild survives
/// the same kill.
///
/// This is the defect reproduction from #109 — a direct-child kill reaches
/// one pid, and the grandchild is a different process. It spawns OUTSIDE the
/// supervisor because `spawn_and_mark_running` now always contains on
/// Windows, which is the point: there is no longer a path that spawns
/// uncontained, so the control has to construct one.
///
/// Its job is to keep `teardown_reaps_the_grandchild` honest. If the
/// grandchild ever dies here, that test is passing for a reason unrelated to
/// the job object and the containment claim is unproven.
#[test]
fn an_uncontained_grandchild_survives_a_direct_child_kill() {
    let dir = ScratchDir::new("teardown-uncontained");
    let pid_file = dir.join("grandchild.pid");
    let mut child = std::process::Command::new(stub_path())
        .env("FAKE_AFT_NEVER_CONNECT", "1")
        .env(
            "FAKE_AFT_GRANDCHILD_PID_FILE",
            pid_file.display().to_string(),
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the uncontained fixture");
    let grandchild = read_grandchild_pid(&pid_file);

    // Exactly what the pre-fix teardown did: kill the direct child.
    child.kill().expect("kill the direct child");
    let _ = child.wait();

    assert!(
        subc_jobobject::process_exists(grandchild),
        "grandchild {grandchild} died with the direct child, so this control no longer \
             distinguishes contained from uncontained teardown and the regression test is \
             passing vacuously"
    );

    // The orphan this control demonstrates is the leak the fix prevents, so
    // the control must not leave one behind.
    kill_tree(grandchild);
}

/// Crash durability: closing the containment handle reaps the tree with no
/// teardown code running at all.
///
/// This is the case `taskkill /T` cannot cover — a daemon that dies cannot
/// call anything — and it is why containment is a kernel property of the
/// handle rather than a step in the drain. Discovered by getting the
/// mutation control wrong: clearing `job` to "disable" containment instead
/// killed the tree, which is the guarantee, not a mistake.
#[tokio::test]
async fn dropping_containment_reaps_the_grandchild() {
    let mut fixture = fixture("drop-containment", "tree-drop");
    let grandchild = fixture.grandchild;

    assert!(subc_jobobject::process_exists(grandchild));

    // No `drain` call, no kill: dropping the handle is the entire mechanism.
    fixture.child.as_mut().expect("child present").job = None;

    assert!(
        subc_jobobject::wait_for_process_exit(grandchild, Duration::from_secs(10)),
        "grandchild {grandchild} survived the containment handle closing, so a daemon \
             crash would leave the tree behind"
    );
}

/// Kill a pid and its tree, then confirm it is gone.
fn kill_tree(pid: u32) {
    let _ = std::process::Command::new("taskkill.exe")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    assert!(
        subc_jobobject::wait_for_process_exit(pid, Duration::from_secs(10)),
        "could not clean up grandchild {pid}"
    );
}
