//! RAII temp-dir guard, and helpers that run test programs under `ckdev-*`
//! names so they are never mistaken for installed `ck-*` production binaries. This
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
/// instead; [`ckdev_binary`] publishes a built binary under such a name.
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
/// binary from a test; [`dev_command`] and [`ckdev_binary`] already do.
///
/// Cargo's own test harness for the `ck` bin target is not refused: cargo names
/// it `target/<profile>/deps/ck-<16 hex digits>`, and re-running that harness
/// (a test that starts its own executable) is cargo's naming, not a copy of a
/// production binary.
pub fn refuse_production_executable(program: &Path) {
    let name = program.file_name().unwrap_or_default();
    assert!(
        !is_production_executable_name(name) || is_cargo_test_harness(program),
        "refusing to run a test process under the production executable name {:?} ({}): \
         `ck-*` and `ck` are reserved for installed binaries; run it through \
         subc_test_support::ckdev_binary so it shows as ckdev-*",
        name,
        program.display()
    );
}

/// Whether `program` is a test harness cargo built for a bin target: a file
/// in a `deps` directory named `<target>-<16 lowercase hex digits>`.
fn is_cargo_test_harness(program: &Path) -> bool {
    let in_deps = program
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|dir| dir == "deps");
    let Some(name) = program.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    let stem = strip_exe_suffix(name).0;
    let hash = stem.rsplit_once('-').map(|(_, hash)| hash).unwrap_or("");
    in_deps
        && hash.len() == 16
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The production-named executables a test may deliberately place, each tied
/// to the one test allowed to place it, as `(test name, file name)`.
///
/// `a_copy_of_ck_on_path_is_neither_probed_recursively_nor_listed` (subc-core
/// `tests/ck_cli.rs`) checks ck's discovery of external `ck-<domain>`
/// executables on PATH, which matches the `ck-` prefix by design, so its two
/// copies of ck must carry that prefix. ck, not the test, starts them, and
/// each refuses the probe and exits within milliseconds. Nothing else is
/// exempt: entries are compared exactly, never as patterns.
const PRODUCTION_NAME_EXEMPTIONS: &[(&str, &str)] = &[
    (
        "a_copy_of_ck_on_path_is_neither_probed_recursively_nor_listed",
        "ck-twin",
    ),
    (
        "a_copy_of_ck_on_path_is_neither_probed_recursively_nor_listed",
        "ck-twin-two",
    ),
];

/// Returns `program` when the test named `test` is the one test allowed to
/// place an executable with `program`'s production name (the exemption
/// table in this crate); panics for any other test or name. A
/// trailing `.exe` is ignored, so the exemption holds on Windows too.
///
/// When the calling thread carries a test name (libtest names each test's
/// thread after it), that name must be `test` as well, so another test cannot
/// borrow the exemption by passing the exempt test's name.
pub fn exempt_production_executable<'a>(test: &str, program: &'a Path) -> &'a Path {
    let file_name = program.file_name().and_then(OsStr::to_str).unwrap_or("");
    let stem = strip_exe_suffix(file_name).0;
    let caller = std::thread::current().name().map(str::to_string);
    let caller_matches = match caller.as_deref() {
        None | Some("main") => true,
        Some(thread) => thread == test || thread.ends_with(&format!("::{test}")),
    };
    assert!(
        caller_matches
            && PRODUCTION_NAME_EXEMPTIONS
                .iter()
                .any(|&(exempt_test, exempt_name)| exempt_test == test && exempt_name == stem),
        "no production-name exemption for {file_name:?} in test {test:?} (running on \
         thread {caller:?}); test processes run as ckdev-* through \
         subc_test_support::ckdev_binary"
    );
    program
}

/// A `Command` for `program` that refuses (panics) when `program` has a
/// production executable name. Use it for every test spawn of a CortexKit
/// binary, with a path made by [`ckdev_binary`].
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
    if len > 4 && name.is_char_boundary(len - 4) && name[len - 4..].eq_ignore_ascii_case(".exe") {
        (&name[..len - 4], &name[len - 4..])
    } else {
        (name, "")
    }
}

/// Publish `built` (a binary cargo built, such as `CARGO_BIN_EXE_ck-subc`)
/// under its `ckdev-` name (see [`ckdev_file_name`]) and return that path, so
/// the process a test starts from it is listed as `ckdev-*`, never as a
/// production `ck-*` binary. A binary already named `ckdev-*` is returned
/// unchanged.
///
/// The path is content-addressed: `/tmp/subc-ckdev/<digest>/<ckdev name>`
/// (the system temp directory instead of `/tmp` off Unix), where the digest
/// covers the name and the bytes. Every test, process and run asking for the
/// same build gets the same file, and a rebuilt binary gets a new directory.
/// Three measurements on macOS shaped this:
///
/// - a fresh executable under the per-user temp directory (`$TMPDIR`) can
///   stall its first system-policy evaluation for 40+ minutes, while the same
///   bytes under `/tmp` answered in about 40 ms, so publishing happens under
///   `/tmp`, once per build;
/// - privacy-trampoline probes executed through hard links to
///   `target/debug/ck-subc` were intermittently killed with SIGKILL (1 of 6
///   subc-core `ck_cli` runs, and 3 in about 21 earlier ones), while the cargo
///   path and copies stayed clean (0 of 6 each), so the published file is a
///   copy, never a link (why the links were killed is not established);
/// - a fresh copy per CLI command slowed tests enough to push byte-exact
///   "just now" ages past one second, so a published file is reused.
///
/// A copy keeps the binary's embedded code signature and its exec bit, but it
/// is a different inode from `built`, so a test comparing a process's
/// executable identity must compare it with the returned path. Publishing
/// stages the copy in a private directory and renames that directory into
/// place, so a reader never sees a partial file. Tests never write to a
/// published file: on Unix it and its directory are read-only, and a reused
/// file is re-hashed (once per process) before it is trusted.
pub fn ckdev_binary(built: impl AsRef<Path>) -> PathBuf {
    ckdev_binary_at(built.as_ref(), &publish_root())
}

/// Where published `ckdev-` binaries live. `/tmp`, not `$TMPDIR`: see
/// [`ckdev_binary`].
fn publish_root() -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from("/tmp/subc-ckdev")
    }
    #[cfg(not(unix))]
    {
        std::env::temp_dir().join("subc-ckdev")
    }
}

fn ckdev_binary_at(built: &Path, root: &Path) -> PathBuf {
    let file_name = built
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_else(|| panic!("built binary has no UTF-8 file name: {}", built.display()));
    let dev_name = ckdev_file_name(file_name);
    if dev_name == file_name {
        return built.to_path_buf();
    }
    let placed = publish(built, &dev_name, root).unwrap_or_else(|error| {
        panic!(
            "could not publish {} as {dev_name} under {}: {error}",
            built.display(),
            root.display()
        )
    });
    refuse_production_executable(&placed);
    placed
}

/// Hex digests of files already hashed by this process, keyed by path, size
/// and modification time, so a binary started hundreds of times is read once.
fn digest_cache() -> &'static std::sync::Mutex<std::collections::HashMap<DigestKey, String>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<DigestKey, String>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

type DigestKey = (PathBuf, String, u64, Option<std::time::SystemTime>);

/// The content address: the first 32 hex digits of SHA-256 over the
/// published name, a NUL, and the file's bytes.
fn content_digest(path: &Path, dev_name: &str) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    let metadata = fs::metadata(path)?;
    let key = (
        path.to_path_buf(),
        dev_name.to_string(),
        metadata.len(),
        metadata.modified().ok(),
    );
    if let Some(digest) = digest_cache().lock().unwrap().get(&key) {
        return Ok(digest.clone());
    }
    let mut hasher = Sha256::new();
    hasher.update(dev_name.as_bytes());
    hasher.update([0u8]);
    let mut file = fs::File::open(path)?;
    io::copy(&mut file, &mut hasher)?;
    let digest: String = hasher.finalize()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    digest_cache().lock().unwrap().insert(key, digest.clone());
    Ok(digest)
}

fn publish(built: &Path, dev_name: &str, root: &Path) -> io::Result<PathBuf> {
    let digest = content_digest(built, dev_name)?;
    prepare_root(root)?;
    let dir = root.join(&digest);
    let placed = dir.join(dev_name);
    if dir.exists() {
        verify_published(&placed, dev_name, &digest)?;
        return Ok(placed);
    }
    prune_stale(root);
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let staging = root.join(format!(".staging-{}-{nonce}", std::process::id()));
    fs::create_dir(&staging)?;
    let staged = staging.join(dev_name);
    let result = (|| {
        copy_executable(built, &staged)?;
        // Permissions are set by path (chmod), so no writable descriptor is
        // held; on Unix the published file is read-only and executable.
        fs::set_permissions(&staged, published_permissions(built)?)?;
        if content_digest(&staged, dev_name)? != digest {
            return Err(io::Error::other(format!(
                "{} changed while it was being published",
                built.display()
            )));
        }
        match fs::rename(&staging, &dir) {
            Ok(()) => {
                seal_dir(&dir)?;
                Ok(())
            }
            // Another process published the same build first; its file is
            // checked below like any reused one.
            Err(_) if dir.exists() => Ok(()),
            Err(error) => Err(error),
        }
    })();
    if staging.exists() {
        remove_published(&staging);
    }
    result?;
    verify_published(&placed, dev_name, &digest)?;
    Ok(placed)
}

/// Re-hashes a published file before trusting it: a partial or altered file
/// is refused, never run.
fn verify_published(placed: &Path, dev_name: &str, digest: &str) -> io::Result<()> {
    let found = content_digest(placed, dev_name)?;
    if found == digest {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "published {} does not match its content address {digest} (found {found}); \
             remove {} to republish it",
            placed.display(),
            placed.parent().unwrap_or(placed).display()
        )))
    }
}

/// Creates the publish root, and refuses one this user does not own or that
/// others can write: `/tmp` is shared, and a planted file there would be run.
fn prepare_root(root: &Path) -> io::Result<()> {
    if !root.exists() {
        fs::create_dir_all(root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::symlink_metadata(root)?;
        let uid = rustix::process::getuid().as_raw();
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
            return Err(io::Error::other(format!(
                "{} must be a directory owned by uid {uid} and writable by no one else",
                root.display()
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn published_permissions(built: &Path) -> io::Result<fs::Permissions> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(built)?.permissions().mode();
    Ok(fs::Permissions::from_mode(mode & 0o555))
}

#[cfg(not(unix))]
fn published_permissions(built: &Path) -> io::Result<fs::Permissions> {
    Ok(fs::metadata(built)?.permissions())
}

/// Makes a published directory read-only, so nothing can be renamed into it
/// or removed from it.
fn seal_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o555))?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Removes a staging or published directory, restoring the write permission
/// sealing took away.
fn remove_published(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    let _ = fs::remove_dir_all(dir);
}

/// Published builds are kept this long after publishing; older ones are
/// removed when a new build is published. A process still running one keeps
/// its open image, and asking again republishes it.
const PUBLISHED_RETENTION: std::time::Duration = std::time::Duration::from_secs(3 * 24 * 60 * 60);

fn prune_stale(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > PUBLISHED_RETENTION);
        if stale {
            remove_published(&entry.path());
        }
    }
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
        for name in [
            "ckdev-subc",
            "ckdev-ck.exe",
            "cksum",
            "fake-aft-stub",
            "subc",
        ] {
            assert!(
                !is_production_executable_name(OsStr::new(name)),
                "{name} is not a production executable name"
            );
        }
    }

    /// The guard itself: the spawn helper refuses a `ck-*` path before any
    /// process starts.
    #[test]
    #[should_panic(
        expected = "refusing to run a test process under the production executable name"
    )]
    fn dev_command_refuses_a_production_named_binary() {
        let _ = dev_command(Path::new("/nonexistent/target/debug/ck-subc"));
    }

    #[test]
    fn cargo_test_harness_for_the_ck_bin_is_not_refused() {
        let _ = dev_command(Path::new("/w/target/debug/deps/ck-0123456789abcdef"));
        let _ = dev_command(Path::new("/w/target/debug/deps/ck-0123456789abcdef.exe"));
        for refused in [
            "/w/target/debug/ck-0123456789abcdef",
            "/w/target/debug/deps/ck-subc",
            "/w/target/debug/deps/ck-0123456789ABCDEF",
            "/w/target/debug/deps/ck-0123456789abcde",
        ] {
            assert!(
                std::panic::catch_unwind(|| dev_command(Path::new(refused))).is_err(),
                "{refused} must be refused"
            );
        }
    }

    const EXEMPT_TEST: &str = "a_copy_of_ck_on_path_is_neither_probed_recursively_nor_listed";

    /// The exemption admits exactly the two twin copies, and only on the
    /// thread of the one test it names.
    #[test]
    fn the_exemption_admits_only_the_named_test_and_its_two_copies() {
        let outcomes = std::thread::Builder::new()
            .name(EXEMPT_TEST.to_string())
            .spawn(|| {
                [
                    "ck-twin",
                    "ck-twin-two",
                    "ck-twin.exe",
                    "ck-twin-three",
                    "ck-subc",
                    "ck",
                ]
                .map(|name| {
                    let path = Path::new("/fixture/bin").join(name);
                    std::panic::catch_unwind(|| {
                        exempt_production_executable(EXEMPT_TEST, &path);
                    })
                    .is_ok()
                })
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(outcomes, [true, true, true, false, false, false]);
        // Another test cannot borrow the exemption by naming the exempt test:
        // this thread carries this test's own name.
        assert!(std::panic::catch_unwind(|| {
            exempt_production_executable(EXEMPT_TEST, Path::new("/fixture/bin/ck-twin"));
        })
        .is_err());
    }

    /// A publish root inside a test temp dir. Published directories are
    /// sealed read-only, so the guard restores write permission before the
    /// temp dir removes the tree.
    #[cfg(unix)]
    struct PublishRoot {
        temp: TestTempDir,
    }

    #[cfg(unix)]
    impl PublishRoot {
        fn new(label: &str) -> Self {
            let temp = TestTempDir::new(label);
            fs::create_dir_all(temp.join("root")).unwrap();
            Self { temp }
        }

        fn path(&self) -> PathBuf {
            self.temp.join("root")
        }
    }

    #[cfg(unix)]
    impl Drop for PublishRoot {
        fn drop(&mut self) {
            if let Ok(entries) = fs::read_dir(self.path()) {
                for entry in entries.flatten() {
                    remove_published(&entry.path());
                }
            }
        }
    }

    /// The guard wired through the helper: binaries built as `ck-subc` and `ck`
    /// are published under `ckdev-*` names and the spawn helper runs them. If
    /// the helper handed back the built path, `dev_command` would refuse it
    /// here.
    #[cfg(unix)]
    #[test]
    fn a_placed_production_binary_spawns_under_its_ckdev_name() {
        let build = TestTempDir::new("ckdev-guard-build");
        let root = PublishRoot::new("ckdev-guard-root");
        for (name, published) in [("ck-subc", "ckdev-subc"), ("ck", "ckdev-ck")] {
            let built = build.join(name);
            write_script(&built, &format!("#!/bin/sh\necho {name}\n"));
            let placed = ckdev_binary_at(&built, &root.path());
            let output = dev_command(&placed).output().unwrap();
            assert_eq!(String::from_utf8_lossy(&output.stdout), format!("{name}\n"));
            assert_eq!(placed.file_name().unwrap(), published);
            assert_eq!(
                placed.parent().and_then(Path::parent),
                Some(root.path().as_path())
            );
        }
    }

    /// Two requests for the same build, even from different build paths, get
    /// the same published file; a changed binary gets a new one.
    #[cfg(unix)]
    #[test]
    fn the_same_build_shares_one_published_path_and_a_changed_build_gets_another() {
        let first = TestTempDir::new("ckdev-address-first");
        let second = TestTempDir::new("ckdev-address-second");
        let root = PublishRoot::new("ckdev-address-root");
        let (a, b) = (first.join("ck-subc"), second.join("ck-subc"));
        write_script(&a, "#!/bin/sh\necho one\n");
        write_script(&b, "#!/bin/sh\necho one\n");
        let placed = ckdev_binary_at(&a, &root.path());
        assert_eq!(ckdev_binary_at(&a, &root.path()), placed);
        assert_eq!(ckdev_binary_at(&b, &root.path()), placed);

        let changed = TestTempDir::new("ckdev-address-changed");
        let c = changed.join("ck-subc");
        write_script(&c, "#!/bin/sh\necho two\n");
        let republished = ckdev_binary_at(&c, &root.path());
        assert_ne!(republished, placed);
        assert_eq!(republished.file_name(), placed.file_name());
        let output = dev_command(&republished).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), "two\n");
        // The first build is still published, unchanged.
        let output = dev_command(&placed).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), "one\n");
    }

    /// The published file is a read-only copy in a read-only directory: its
    /// own inode, the same bytes, executable, and no staging left behind.
    #[cfg(unix)]
    #[test]
    fn a_published_binary_is_a_sealed_copy() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let build = TestTempDir::new("ckdev-copy-build");
        let root = PublishRoot::new("ckdev-copy-root");
        let built = build.join("ck-subc");
        write_script(&built, "#!/bin/sh\necho copied\n");
        let placed = ckdev_binary_at(&built, &root.path());
        let (source, copy) = (
            fs::metadata(&built).unwrap(),
            fs::metadata(&placed).unwrap(),
        );
        assert_ne!(
            (source.dev(), source.ino()),
            (copy.dev(), copy.ino()),
            "a published binary must not share the built binary's inode"
        );
        assert_eq!(fs::read(&built).unwrap(), fs::read(&placed).unwrap());
        assert_eq!(copy.permissions().mode() & 0o777, 0o555);
        let dir = placed.parent().unwrap();
        assert_eq!(
            fs::metadata(dir).unwrap().permissions().mode() & 0o777,
            0o555
        );
        assert!(
            fs::OpenOptions::new().write(true).open(&placed).is_err(),
            "a published binary must not be writable"
        );
        let entries: Vec<_> = fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, [dir.file_name().unwrap()]);
    }

    /// A published file that no longer matches its address is refused, not
    /// run, and so is a publish root other users can write.
    #[cfg(unix)]
    #[test]
    fn altered_publications_and_shared_roots_are_refused() {
        use std::os::unix::fs::PermissionsExt;
        let build = TestTempDir::new("ckdev-refuse-build");
        let root = PublishRoot::new("ckdev-refuse-root");
        let built = build.join("ck-bus");
        write_script(&built, "#!/bin/sh\necho genuine\n");
        let placed = ckdev_binary_at(&built, &root.path());
        let dir = placed.parent().unwrap().to_path_buf();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&placed, fs::Permissions::from_mode(0o755)).unwrap();
        let altered = build.join("altered");
        write_script(&altered, "#!/bin/sh\necho planted\n");
        fs::remove_file(&placed).unwrap();
        copy_executable(&altered, &placed).unwrap();
        // A fresh process would hash it anew; drop this one's memo too.
        digest_cache().lock().unwrap().clear();
        let error = publish(&built, "ckdev-bus", &root.path()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not match its content address"),
            "{error}"
        );

        let open = PublishRoot::new("ckdev-open-root");
        fs::set_permissions(open.path(), fs::Permissions::from_mode(0o777)).unwrap();
        let error = publish(&built, "ckdev-bus", &open.path()).unwrap_err();
        assert!(
            error.to_string().contains("writable by no one else"),
            "{error}"
        );
    }

    #[test]
    fn an_already_ckdev_binary_is_returned_unchanged() {
        let built = Path::new("/nonexistent/ckdev-subc");
        assert_eq!(ckdev_binary(built), built);
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
