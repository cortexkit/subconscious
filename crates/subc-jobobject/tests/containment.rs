//! Containment tests: a grandchild must not outlive teardown.
//!
//! These are the evidence for the fix. The failure they defend against is a
//! module whose helper process survives the module: on this machine the Synapse
//! embedding module's CUDA worker holds ~2.2 GB of VRAM, so a leaked grandchild
//! is a leaked GPU allocation, and a day of restarts compounds it.
//!
//! Every assertion names a **pid**, not "the job is empty". A test that only
//! checked the job's own count would pass against a job that was never populated
//! and against a tree that leaked but was never assigned.

#![cfg(windows)]

use std::{
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

use subc_jobobject::{process_exists, wait_for_process_exit, JobObject};

/// How long a killed tree is given to leave the process table.
///
/// `TerminateJobObject` returns once members are signalled, not once they are
/// reaped, so teardown is asynchronous and an immediate assertion would be
/// racing the kernel.
const EXIT_DEADLINE: Duration = Duration::from_secs(10);

/// How long the grandchild's pid file is given to appear.
const GRANDCHILD_APPEARANCE_DEADLINE: Duration = Duration::from_secs(10);

/// The fixture binary, expected beside this test executable.
///
/// The existence check is here because a bare spawn failure reads as a broken
/// test rather than an unbuilt dependency: `cargo test -p subc-jobobject` builds
/// `[[bin]]` targets, `--lib` does not.
fn fixture_path() -> PathBuf {
    let mut path = std::env::current_exe().expect("current_exe available in tests");
    path.pop();
    path.pop();
    path.push("jobobject-fixture.exe");
    assert!(
        path.exists(),
        "jobobject-fixture not built at {}: run `cargo test -p subc-jobobject` \
         (which builds [[bin]] targets) rather than `--lib`",
        path.display()
    );
    path
}

/// A spawn whose parent has already produced its grandchild.
struct Fixture {
    child: std::process::Child,
    grandchild_pid: u32,
    _dir: TempDir,
}

/// Minimal RAII temp dir, matching the daemon's `TestTempDir` convention of
/// keeping orphans attributable rather than hand-assembled.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir()
            .join("subc-jobobject-tests")
            .join(format!("{label}-{}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create test temp dir");
        Self(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // A panicking test's evidence must outlive it, so the tree is kept and
        // its path printed rather than silently removed.
        if std::thread::panicking() {
            eprintln!("fixture temp dir kept for inspection: {}", self.0.display());
            return;
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Spawn the fixture parent, contained, and wait for its grandchild to appear.
///
/// The wait is on the grandchild's published pid, not a sleep: the test must not
/// assert against a tree that has not finished forming, or it would pass for the
/// wrong reason.
fn spawn_contained_fixture(label: &str) -> (Fixture, JobObject) {
    let dir = TempDir::new(label);
    let grandchild_pid_file = dir.join("grandchild.pid");

    let mut command = Command::new(fixture_path());
    command
        .env("SUBC_JOBOBJECT_FIXTURE_MODE", "parent")
        .env("SUBC_JOBOBJECT_GRANDCHILD_PID_FILE", &grandchild_pid_file)
        // The fixture parks; it must not hold this test's stdout/stderr open.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let (child, job) = {
        let contained = subc_jobobject::spawn_contained(&mut command).expect("spawn fixture");
        (contained.child, contained.job)
    };

    let grandchild_pid = wait_for_pid_file(&grandchild_pid_file)
        .unwrap_or_else(|| panic!("grandchild pid file never appeared at {grandchild_pid_file:?}"));

    (
        Fixture {
            child,
            grandchild_pid,
            _dir: dir,
        },
        job,
    )
}

/// Poll for the pid file and parse it.
fn wait_for_pid_file(path: &PathBuf) -> Option<u32> {
    let deadline = std::time::Instant::now() + GRANDCHILD_APPEARANCE_DEADLINE;
    loop {
        if let Ok(contents) = std::fs::read_to_string(path) {
            if let Ok(pid) = contents.trim().parse() {
                return Some(pid);
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The fix: killing the job takes the grandchild with it.
///
/// This is the assertion the whole change exists for. Before containment the
/// grandchild survived — it was a separate process whose parent had been
/// `TerminateProcess`'d, so it could not be reached by any walk of the tree.
#[test]
fn terminating_the_job_kills_the_grandchild() {
    let (mut fixture, job) = spawn_contained_fixture("terminate");

    let parent_pid = fixture.child.id();
    assert!(
        process_exists(fixture.grandchild_pid),
        "grandchild {} must be alive before teardown, or this test proves nothing",
        fixture.grandchild_pid
    );

    job.terminate().expect("terminate job");

    assert!(
        wait_for_process_exit(fixture.grandchild_pid, EXIT_DEADLINE),
        "grandchild {} survived job termination; the tree was not contained",
        fixture.grandchild_pid
    );
    assert!(
        wait_for_process_exit(parent_pid, EXIT_DEADLINE),
        "parent {parent_pid} survived job termination"
    );

    let _ = fixture.child.wait();
}

/// The crash-durability guarantee: the tree dies when the handle closes, with no
/// code running to ask it to.
///
/// This is the case `taskkill /T` cannot cover at all — a daemon that dies
/// cannot call anything, so containment has to be a kernel property of the
/// handle rather than a teardown step.
#[test]
fn dropping_the_job_kills_the_grandchild() {
    let (mut fixture, job) = spawn_contained_fixture("drop");

    assert!(process_exists(fixture.grandchild_pid));

    // No `terminate` call: closing the last handle is the whole mechanism.
    drop(job);

    assert!(
        wait_for_process_exit(fixture.grandchild_pid, EXIT_DEADLINE),
        "grandchild {} survived the job handle closing",
        fixture.grandchild_pid
    );

    let _ = fixture.child.wait();
}

/// The mutation control the review asked for: with nothing assigned, the
/// grandchild survives — and this test goes red if that ever stops being true.
///
/// Without this, a passing `terminating_the_job_kills_the_grandchild` would not
/// distinguish "the job contained the tree" from "the fixture's grandchild died
/// for some unrelated reason". Here the child is spawned and resumed exactly as
/// the fix does, but never assigned, so the only difference is membership.
#[test]
fn an_unassigned_grandchild_survives_teardown() {
    let dir = TempDir::new("unassigned");
    let grandchild_pid_file = dir.join("grandchild.pid");

    let mut command = Command::new(fixture_path());
    command
        .env("SUBC_JOBOBJECT_FIXTURE_MODE", "parent")
        .env("SUBC_JOBOBJECT_GRANDCHILD_PID_FILE", &grandchild_pid_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    subc_jobobject::suspend_on_create(&mut command);
    let mut child = command.spawn().expect("spawn unassigned fixture");
    // Resumed, so the fixture actually runs and forms its tree — but no job.
    subc_jobobject::resume_main_thread(child.id()).expect("resume");

    let grandchild_pid =
        wait_for_pid_file(&grandchild_pid_file).expect("grandchild pid file never appeared");
    assert!(process_exists(grandchild_pid), "grandchild must be alive");

    // Kill only the direct child, exactly as the pre-fix teardown did.
    child.kill().expect("kill direct child");
    let _ = child.wait();

    assert!(
        process_exists(grandchild_pid),
        "grandchild {grandchild_pid} died with its parent, which would mean this \
         control no longer distinguishes contained from uncontained — the \
         regression test above would then be passing vacuously"
    );

    // Leave nothing behind: this is the leak the fix prevents, so the test that
    // demonstrates it must clean it up itself.
    let mut cleanup = Command::new("taskkill.exe");
    cleanup
        .args(["/PID", &grandchild_pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let _ = cleanup.status();
    assert!(
        wait_for_process_exit(grandchild_pid, EXIT_DEADLINE),
        "control test could not clean up grandchild {grandchild_pid}"
    );
}

/// Assignment must happen while the child is suspended, so the job holds the
/// process before it can create anything.
///
/// Proves the ordering contract rather than assuming it: a child assigned after
/// it ran would already have had the chance to spawn an escapee.
#[test]
fn a_suspended_child_is_assigned_before_it_runs() {
    let dir = TempDir::new("suspended");
    let pid_file = dir.join("grandchild.pid");

    let mut command = Command::new(fixture_path());
    command
        .env("SUBC_JOBOBJECT_FIXTURE_MODE", "parent")
        .env("SUBC_JOBOBJECT_GRANDCHILD_PID_FILE", &pid_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    subc_jobobject::suspend_on_create(&mut command);
    let child = command.spawn().expect("spawn suspended fixture");

    // While suspended the fixture has run nothing, so it cannot have spawned
    // its grandchild yet. If this file exists here, `CREATE_SUSPENDED` was not
    // applied and the containment window is open.
    assert!(
        !pid_file.exists(),
        "the child ran before it was resumed; the suspended-create contract is broken"
    );

    let job = JobObject::new().expect("create job");
    job.assign(&child).expect("assign while suspended");
    assert_eq!(
        job.process_count().expect("count"),
        1,
        "only the child itself is contained at this point"
    );

    subc_jobobject::resume_main_thread(child.id()).expect("resume");

    let grandchild_pid = wait_for_pid_file(&pid_file).expect("grandchild pid file never appeared");
    assert!(process_exists(grandchild_pid));

    job.terminate().expect("terminate");
    assert!(
        wait_for_process_exit(grandchild_pid, EXIT_DEADLINE),
        "grandchild {grandchild_pid} survived: it escaped the suspension window"
    );

    let mut child = child;
    let _ = child.wait();
}

/// A contained child gets no console window (#131).
///
/// Without `CREATE_NO_WINDOW` a console-subsystem child either opens its own
/// console (under a daemon with none) or attaches to its parent's (under this
/// test runner). Either way it has a console window, and closing it ends the
/// child with `STATUS_CONTROL_C_EXIT`. So "has no console at all" is the
/// assertion that holds only when the flag is set.
#[test]
fn a_contained_child_has_no_console_window() {
    let dir = TempDir::new("console");
    let report = dir.join("console.report");
    let mut command = Command::new(fixture_path());
    command
        .env("SUBC_JOBOBJECT_FIXTURE_MODE", "console")
        .env("SUBC_JOBOBJECT_CONSOLE_REPORT", &report)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut contained = subc_jobobject::spawn_contained(&mut command).expect("spawn fixture");
    let status = contained
        .child
        .wait()
        .expect("wait for the console fixture");
    assert!(status.success(), "console fixture failed: {status:?}");
    let reported = std::fs::read_to_string(&report)
        .unwrap_or_else(|error| panic!("console report missing at {report:?}: {error}"));
    assert_eq!(
        reported, "no-console",
        "a contained child must be created with CREATE_NO_WINDOW"
    );
}
