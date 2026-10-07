//! RAII temp-dir guard and test-process naming for workspace tests. This
//! crate is used only as a dev-dependency.

use std::{
    ffi::OsStr,
    fs, io,
    ops::Deref,
    path::{Path, PathBuf},
    process::Command,
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

/// Executable names a test must never run a process under.
///
/// macOS (Activity Monitor, `ps -o comm`) and other process listings show a
/// process by its executable's file name. Names that start with `ck-`, plus the
/// `ck` CLI itself, belong to the production binaries installed in the
/// CortexKit bin directory, so a test daemon running as `ck-subc` looks exactly
/// like a second production daemon. Test processes run under `ckdev-<name>`
/// instead; [`ckdev_binary_in`] makes such a name for a built binary.
pub fn is_production_executable_name(file_name: &OsStr) -> bool {
    let Some(name) = file_name.to_str() else {
        // Production names are ASCII; a non-UTF-8 name cannot be one.
        return false;
    };
    let stem = strip_exe_suffix(name).0.to_ascii_lowercase();
    stem == "ck" || stem.starts_with("ck-")
}

/// Panics when `program` would run under a production executable name (see
/// [`is_production_executable_name`]). Call it before spawning any CortexKit
/// binary from a test; [`dev_command`] and [`ckdev_binary_in`] already do.
pub fn refuse_production_executable(program: &Path) {
    let name = program.file_name().unwrap_or_default();
    assert!(
        !is_production_executable_name(name),
        "refusing to run a test process under the production executable name {:?} ({}): \
         `ck-*` and `ck` are reserved for installed binaries; run it through \
         subc_test_support::ckdev_binary_in or CkdevBinary so it shows as ckdev-*",
        name,
        program.display()
    );
}

/// A `Command` for `program` that refuses (panics) when `program` has a
/// production executable name. Use it for every test spawn of a CortexKit
/// binary, with a path made by [`ckdev_binary_in`] or [`CkdevBinary`].
pub fn dev_command(program: impl AsRef<Path>) -> Command {
    let program = program.as_ref();
    refuse_production_executable(program);
    Command::new(program)
}

/// The `ckdev-` name for a built binary's file name: `ck-subc` becomes
/// `ckdev-subc`, `ck` becomes `ckdev-ck`, any other name `n` becomes
/// `ckdev-n`, and a name that already starts with `ckdev-` is returned as it
/// is. A trailing `.exe` stays at the end, so Windows still runs the result.
pub fn ckdev_file_name(file_name: &str) -> String {
    let (stem, exe) = strip_exe_suffix(file_name);
    if stem.starts_with("ckdev-") {
        return file_name.to_string();
    }
    let base = match stem.strip_prefix("ck-") {
        Some(rest) => rest,
        None => stem,
    };
    format!("ckdev-{base}{exe}")
}

fn strip_exe_suffix(name: &str) -> (&str, &str) {
    let len = name.len();
    if len > 4 && name.is_char_boundary(len - 4) && name[len - 4..].eq_ignore_ascii_case(".exe")
    {
        (&name[..len - 4], &name[len - 4..])
    } else {
        (name, "")
    }
}

/// Place `built` (a binary cargo built, such as `CARGO_BIN_EXE_ck-subc`) in
/// `scratch` under its `ckdev-` name (see [`ckdev_file_name`]) and return that
/// path, so the process a test starts from it is listed as `ckdev-*`, never as
/// a production `ck-*` binary. `scratch` is a directory the test owns and keeps
/// for as long as anything may still start the binary.
///
/// The placement is a hard link: the same file, so the same inode, code
/// signature and executable identity a provenance check compares. When the
/// scratch directory is on another filesystem the binary is copied instead; a
/// copy keeps its embedded code signature and gets the source's permissions,
/// exec bit included, but it is a different inode, so a test comparing a
/// process's executable identity must compare it with the returned path, not
/// with `built`.
///
/// A binary already named `ckdev-*` is returned unchanged. Calling this again
/// for the same binary and scratch directory returns the existing placement.
pub fn ckdev_binary_in(built: impl AsRef<Path>, scratch: impl AsRef<Path>) -> PathBuf {
    let built = built.as_ref();
    let file_name = built
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_else(|| panic!("built binary has no UTF-8 file name: {}", built.display()));
    let dev_name = ckdev_file_name(file_name);
    if dev_name == file_name {
        return built.to_path_buf();
    }
    let placed = scratch.as_ref().join(&dev_name);
    if !placed.exists() {
        place_binary(built, &placed).unwrap_or_else(|error| {
            panic!(
                "could not place {} as {}: {error}",
                built.display(),
                placed.display()
            )
        });
    }
    refuse_production_executable(&placed);
    placed
}

fn place_binary(built: &Path, placed: &Path) -> io::Result<()> {
    if let Some(parent) = placed.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::hard_link(built, placed) {
        Ok(()) => return Ok(()),
        // Another thread of the same test placed it first; that file is the
        // same build.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        // Cross-device scratch directories and filesystems without hard links
        // fall through to a copy.
        Err(_) => {}
    }
    copy_into_place(built, placed)
}

fn copy_into_place(built: &Path, placed: &Path) -> io::Result<()> {
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let staging = placed.with_file_name(format!(
        ".{}.partial-{}-{nonce}",
        placed.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    copy_executable(built, &staging)?;
    // Permissions are set by path (chmod), so no writable descriptor is held.
    fs::set_permissions(&staging, fs::metadata(built)?.permissions())?;
    if placed.exists() {
        // A concurrent placement won; keep it rather than replacing a file
        // that may already be running.
        let _ = fs::remove_file(&staging);
        return Ok(());
    }
    fs::rename(&staging, placed)
}

/// Copies through a `cp` child on Unix: a writable descriptor held by this
/// multi-threaded test process would be inherited by a child another thread
/// forks at that moment, and executing the copy while that child still holds
/// it fails with "text file busy".
fn copy_executable(src: &Path, dst: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let status = Command::new("cp").arg(src).arg(dst).status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("cp exited with {status}")))
        }
    }
    #[cfg(not(unix))]
    {
        fs::copy(src, dst).map(|_| ())
    }
}

/// A built binary placed under its `ckdev-` name (see [`ckdev_binary_in`]) in a
/// scratch directory this value owns. Keep it alive for as long as anything
/// may start the binary: dropping it removes the directory.
pub struct CkdevBinary {
    path: PathBuf,
    _scratch: TestTempDir,
}

impl CkdevBinary {
    /// Place `built` under its `ckdev-` name in a new scratch directory.
    pub fn new(built: impl AsRef<Path>) -> Self {
        let scratch = TestTempDir::new("ckdev-bin");
        let path = ckdev_binary_in(built, scratch.path());
        Self {
            path,
            _scratch: scratch,
        }
    }

    /// The placed binary.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A `Command` that runs the placed binary.
    pub fn command(&self) -> Command {
        dev_command(&self.path)
    }
}

impl AsRef<Path> for CkdevBinary {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<OsStr> for CkdevBinary {
    fn as_ref(&self) -> &OsStr {
        self.path.as_os_str()
    }
}

impl Deref for CkdevBinary {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
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

    #[test]
    fn ckdev_names_drop_the_production_prefix_and_keep_exe() {
        assert_eq!(ckdev_file_name("ck-subc"), "ckdev-subc");
        assert_eq!(ckdev_file_name("ck-subc-mcp"), "ckdev-subc-mcp");
        assert_eq!(ckdev_file_name("ck-bus.exe"), "ckdev-bus.exe");
        assert_eq!(ckdev_file_name("ck"), "ckdev-ck");
        assert_eq!(ckdev_file_name("ck.exe"), "ckdev-ck.exe");
        assert_eq!(ckdev_file_name("ck-under-test"), "ckdev-under-test");
        assert_eq!(ckdev_file_name("fake-aft-stub"), "ckdev-fake-aft-stub");
        assert_eq!(ckdev_file_name("ckdev-subc"), "ckdev-subc");
        assert_eq!(ckdev_file_name("ckdev-subc.exe"), "ckdev-subc.exe");
    }

    #[test]
    fn production_names_are_recognised() {
        for name in ["ck-subc", "ck-bus.exe", "CK-SUBC.EXE", "ck", "ck.exe"] {
            assert!(
                is_production_executable_name(OsStr::new(name)),
                "{name} is a production executable name"
            );
        }
        for name in ["ckdev-subc", "ckdev-ck.exe", "cksum", "fake-aft-stub", "subc"] {
            assert!(
                !is_production_executable_name(OsStr::new(name)),
                "{name} is not a production executable name"
            );
        }
    }

    /// The guard itself: the spawn helper refuses a `ck-*` path before any
    /// process starts.
    #[test]
    #[should_panic(expected = "refusing to run a test process under the production executable name")]
    fn dev_command_refuses_a_production_named_binary() {
        let _ = dev_command(Path::new("/nonexistent/target/debug/ck-subc"));
    }

    /// The guard wired through the helper: a binary built as `ck-*` is placed
    /// under a `ckdev-*` name and the spawn helper runs it. If the helper
    /// handed back the built path, `dev_command` would refuse it here.
    #[cfg(unix)]
    #[test]
    fn a_placed_production_binary_spawns_under_its_ckdev_name() {
        let build = TestTempDir::new("ckdev-guard-build");
        let built = build.join("ck-subc");
        write_script(&built, "#!/bin/sh\necho placed\n");
        let scratch = TestTempDir::new("ckdev-guard-scratch");
        let placed = ckdev_binary_in(&built, scratch.path());
        let output = dev_command(&placed).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), "placed\n");
        assert_eq!(placed.file_name().unwrap(), "ckdev-subc");
        assert_eq!(placed.parent(), Some(scratch.path()));
    }

    #[cfg(unix)]
    #[test]
    fn placement_is_a_hard_link_to_the_same_inode() {
        use std::os::unix::fs::MetadataExt;
        let build = TestTempDir::new("ckdev-link-build");
        let built = build.join("ck-bus");
        write_script(&built, "#!/bin/sh\n");
        let scratch = TestTempDir::new("ckdev-link-scratch");
        let placed = ckdev_binary_in(&built, scratch.path());
        let (a, b) = (fs::metadata(&built).unwrap(), fs::metadata(&placed).unwrap());
        assert_eq!((a.dev(), a.ino()), (b.dev(), b.ino()));
        // A second call reuses the placement instead of failing on it.
        assert_eq!(ckdev_binary_in(&built, scratch.path()), placed);
    }

    /// The cross-filesystem fallback: a copy with the exec bit preserved.
    #[cfg(unix)]
    #[test]
    fn the_copy_fallback_keeps_the_exec_bit() {
        use std::os::unix::fs::PermissionsExt;
        let build = TestTempDir::new("ckdev-copy-build");
        let built = build.join("ck-subc");
        write_script(&built, "#!/bin/sh\necho copied\n");
        let scratch = TestTempDir::new("ckdev-copy-scratch");
        let placed = scratch.join("ckdev-subc");
        copy_into_place(&built, &placed).unwrap();
        let mode = fs::metadata(&placed).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "mode {mode:o}");
        let output = dev_command(&placed).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), "copied\n");
        let leftovers: Vec<_> = fs::read_dir(scratch.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, [OsStr::new("ckdev-subc")]);
    }

    #[test]
    fn an_already_ckdev_binary_is_returned_unchanged() {
        let built = Path::new("/nonexistent/ckdev-subc");
        let scratch = TestTempDir::new("ckdev-unchanged");
        assert_eq!(ckdev_binary_in(built, scratch.path()), built);
    }

    #[cfg(unix)]
    #[test]
    fn an_owned_placement_lives_until_dropped() {
        let build = TestTempDir::new("ckdev-owned-build");
        let built = build.join("ck");
        write_script(&built, "#!/bin/sh\n");
        let placed = CkdevBinary::new(&built);
        let path = placed.path().to_path_buf();
        assert_eq!(path.file_name().unwrap(), "ckdev-ck");
        assert!(placed.command().status().unwrap().success());
        drop(placed);
        assert!(!path.exists(), "dropping the placement removes its scratch dir");
    }

    /// Writes through a staging file and a `cp` child, so this multi-threaded
    /// test process never holds a writable descriptor to the file it runs.
    #[cfg(unix)]
    fn write_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let staging = path.with_extension("staging");
        fs::write(&staging, body).unwrap();
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o755)).unwrap();
        copy_executable(&staging, path).unwrap();
        fs::remove_file(&staging).unwrap();
    }
}
