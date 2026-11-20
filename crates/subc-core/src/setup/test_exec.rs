//! Test fixtures that place a file a test is about to execute.
//!
//! The test binary runs tests on many threads, and any of them may `fork()`
//! for a `Command::spawn` at any moment. A child forked while this process
//! holds a writable descriptor on a file inherits that descriptor until the
//! child reaches `execve` (only then does `O_CLOEXEC` close it). If a test
//! executes the file inside that window, the kernel still sees the file open
//! for writing and refuses with `ETXTBSY` ("Text file busy"); renaming the
//! file first does not help, because the inherited descriptor refers to the
//! same inode. The helpers below therefore never open the file for writing in
//! this process: a short-lived child does the writing, so the writable
//! descriptor exists only in that child and is closed when it exits.

use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
};

/// Write `contents` to `path` and mark it executable, for a test that will
/// execute `path`.
///
/// Do not replace this with `fs::write`: the writable descriptor would live in
/// the multithreaded test process, where a child forked by another test thread
/// can inherit it and make this test's later exec of `path` fail with
/// `ETXTBSY`. Here `sh` opens the file and `cat` fills it from a pipe, so this
/// process only ever holds the pipe, which cannot block an exec.
pub(crate) fn write_executable(path: &Path, contents: &[u8]) {
    let mut child = Command::new("sh")
        .args(["-c", r#"cat > "$1""#, "write_executable"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("start sh for an executable fixture");
    {
        let mut stdin = child.stdin.take().expect("pipe to the fixture writer");
        stdin
            .write_all(contents)
            .expect("send executable fixture contents");
        // Dropping the pipe here sends end-of-file so `cat` finishes.
    }
    let status = child.wait().expect("wait for the fixture writer");
    assert!(
        status.success(),
        "writing executable fixture {} failed: {status}",
        path.display()
    );
    mark_executable(path);
}

fn mark_executable(path: &Path) {
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|error| panic!("mark {} executable: {error}", path.display()));
}
