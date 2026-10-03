//! RAII temp-dir guard for workspace tests. This crate is used only as a dev-dependency.

use std::{
    fs,
    ops::Deref,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// RAII guard for a uniquely-named test temp directory.
///
/// The directory is created under `std::env::temp_dir().join("subc-tests")` so
/// every test-owned temp dir lives under one recognizable parent: a future
/// orphan population is one directory listing away from attribution instead of
/// a hand-assembled census.
///
/// On `Drop` the tree is removed — EXCEPT when the thread is panicking
/// (`std::thread::panicking()`): then the tree is left in place and its path is
/// printed to stderr, because a failing test's evidence must outlive it.
pub struct TestTempDir {
    path: PathBuf,
    kept: bool,
}

impl TestTempDir {
    /// Create a new uniquely-named temp dir under `subc-tests/`.
    pub fn new(label: &str) -> Self {
        let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join("subc-tests")
            .join(format!("{label}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&path).expect("create test temp dir");
        Self { path, kept: false }
    }

    /// The directory path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Escape hatch: hand the directory to a child process that outlives the
    /// test. Consumes the guard so `Drop` does not remove the tree.
    pub fn keep(mut self) -> PathBuf {
        self.kept = true;
        self.path.clone()
    }
}

impl AsRef<Path> for TestTempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Deref for TestTempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestTempDir {
    fn drop(&mut self) {
        if self.kept {
            return;
        }
        if std::thread::panicking() {
            // A failing test's evidence must outlive it: leave the tree in
            // place and print the path so the failure is attributable.
            eprintln!("TestTempDir preserved on panic: {}", self.path.display());
            return;
        }
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Whether `pid` is a running process, for tests that assert a process has
/// ended.
///
/// Signal 0 alone also succeeds for a zombie: a process that has exited but
/// not yet been reaped. A child whose parent was killed or has exited is
/// reaped by whatever adopted it (init on Linux), not by the test, so for a
/// moment after it ends it is still found by signal 0. On Linux a zombie
/// therefore counts as gone. Other Unix systems answer by signal 0 only.
#[cfg(unix)]
pub fn process_alive(pid: i32) -> bool {
    let Some(target) = rustix::process::Pid::from_raw(pid) else {
        return false;
    };
    rustix::process::test_kill_process(target).is_ok() && !is_zombie(pid)
}

/// Wait up to `timeout` for `pid` to end, as [`process_alive`] judges it.
/// Returns whether it ended. Use this, not a single [`process_alive`] check,
/// wherever a test asserts that something else has just ended a process: the
/// reaping that makes it disappear happens on another process's schedule.
#[cfg(unix)]
pub fn wait_until_gone(pid: i32, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while process_alive(pid) {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    true
}

#[cfg(target_os = "linux")]
fn is_zombie(pid: i32) -> bool {
    // The state is the first field after the parenthesised command name.
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(')')
                .map(|(_, rest)| rest.trim_start().starts_with('Z'))
        })
        .unwrap_or(false)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn is_zombie(_pid: i32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    /// A child that has exited but that nobody has reaped yet is a zombie:
    /// signal 0 still finds it, and `process_alive` must not. The test is the
    /// child's parent and deliberately does not wait for it until the end.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_exited_unreaped_child_is_not_alive() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let gone = super::wait_until_gone(pid, std::time::Duration::from_secs(5));
        let signal_zero_still_finds_it =
            rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap())
                .is_ok();
        child.wait().unwrap();
        assert!(
            signal_zero_still_finds_it,
            "the child was reaped too early to test the zombie case"
        );
        assert!(
            gone,
            "an exited child awaiting its reaper must count as gone"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_running_child_is_alive() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let alive = super::process_alive(pid);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(alive);
    }

    use super::*;

    #[test]
    fn success_drop_removes_the_tree() {
        let dir = TestTempDir::new("lifecycle-success");
        let path = dir.path().to_path_buf();
        assert!(path.exists());
        drop(dir);
        assert!(!path.exists(), "guard drop must remove the tree");
    }

    #[test]
    fn panic_preserves_the_tree_and_prints_the_path() {
        let path = std::thread::spawn(|| {
            let dir = TestTempDir::new("lifecycle-panic");
            let path = dir.path().to_path_buf();
            // Panic while the guard is alive: Drop runs during unwinding and
            // must preserve the tree.
            panic!("intentional test panic with path {}", path.display());
        })
        .join()
        .expect_err("the spawned thread must panic");
        let message = path
            .downcast_ref::<String>()
            .map(String::as_str)
            .unwrap_or("<non-string panic payload>");
        assert!(
            message.contains("lifecycle-panic"),
            "panic payload should name the dir: {message}"
        );
        // The tree must survive the panicking thread's unwinding.
        let survived = std::env::temp_dir()
            .join("subc-tests")
            .read_dir()
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .find(|name| name.contains("lifecycle-panic"));
        assert!(
            survived.is_some(),
            "a panicking thread's guard must leave its tree in place"
        );
        // Clean up the preserved evidence so the test does not leak.
        let dir = std::env::temp_dir()
            .join("subc-tests")
            .join(survived.unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keep_preserves_without_panic() {
        let dir = TestTempDir::new("lifecycle-keep");
        let path = dir.keep();
        assert!(path.exists(), "keep() must leave the tree in place");
        // The guard was consumed; nothing removes the tree.
        fs::remove_dir_all(&path).unwrap();
    }
}
