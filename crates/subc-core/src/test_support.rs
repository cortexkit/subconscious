//! Test-only RAII temp-dir guard shared by the crate's unit tests and its
//! integration tests.
//!
//! The module is compiled into the library only when the `test-support` feature
//! is enabled (integration tests, via the self dev-dependency) or under
//! `#[cfg(test)]` (the crate's own unit tests). It is never part of a
//! production build.

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

#[cfg(test)]
mod tests {
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
