use std::ffi::OsString;

use super::*;

const NONCE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[cfg(not(unix))]
#[test]
fn windows_reads_only_the_environment_copy_even_when_a_descriptor_is_named() {
    let got = LaunchNonceCell::new()
        .get(|key| match key {
            LAUNCH_NONCE_FD_ENV => Some(OsString::from("3:1")),
            LAUNCH_NONCE_ENV => Some(OsString::from("env-copy")),
            _ => None,
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        (got.value(), got.source()),
        ("env-copy", LaunchNonceSource::Env)
    );
}

#[test]
fn without_the_descriptor_variable_the_environment_copy_is_read() {
    let got = LaunchNonceCell::new()
        .get(|key| (key == LAUNCH_NONCE_ENV).then(|| OsString::from("env-copy")))
        .unwrap()
        .unwrap();
    assert_eq!(
        (got.value(), got.source()),
        ("env-copy", LaunchNonceSource::Env)
    );
    assert_eq!(LaunchNonceCell::new().get(|_| None), Ok(None));
    assert_eq!(
        LaunchNonceCell::new().get(|key| (key == LAUNCH_NONCE_ENV).then(OsString::new)),
        Ok(None),
        "an empty environment copy is no nonce"
    );
}

#[test]
fn debug_never_prints_the_nonce() {
    let nonce = LaunchNonce {
        value: NONCE.to_string(),
        source: LaunchNonceSource::Fd,
    };
    let printed = format!("{nonce:?}");
    assert!(!printed.contains(NONCE), "{printed}");
    assert!(printed.contains("Fd"), "{printed}");
}

#[cfg(unix)]
mod unix_tests {
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
        ffi::OsString,
        fs::{self, File},
        io::Read,
        os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd},
        os::unix::process::CommandExt,
        path::{Path, PathBuf},
        process::{Command, Stdio},
        sync::Arc,
    };

    use super::super::{
        launch_nonce, LaunchNonceCell, LaunchNonceError, LaunchNonceHandoff, LaunchNonceSource,
        LAUNCH_NONCE_ENV, LAUNCH_NONCE_FD_ENV,
    };
    use super::NONCE;

    // These tests open, close and duplicate descriptors by number, and a
    // number one test frees can be reused by another at once. They take this
    // lock so no two of them run together.
    static RAW_FD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        RAW_FD_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn fd_is_open(fd: RawFd) -> bool {
        // SAFETY: F_GETFD only reads the descriptor table.
        #[allow(unsafe_code)]
        unsafe {
            libc::fcntl(fd, libc::F_GETFD) != -1
        }
    }

    /// A pipe holding `bytes` with its write end closed, and its inode.
    fn sealed_pipe(bytes: &[u8]) -> (OwnedFd, u64) {
        let handoff = LaunchNonceHandoff::new(std::str::from_utf8(bytes).unwrap()).unwrap();
        let inode = handoff.inode();
        let fd = handoff.read_end_fd();
        // Take the read end out of the handoff without closing it.
        // SAFETY: dup gives this test its own descriptor for the same pipe.
        #[allow(unsafe_code)]
        let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        assert!(copy >= 0);
        drop(handoff);
        // SAFETY: `copy` is a fresh descriptor nothing else owns.
        #[allow(unsafe_code)]
        (unsafe { OwnedFd::from_raw_fd(copy) }, inode)
    }

    fn naming(value: String) -> impl FnMut(&str) -> Option<OsString> {
        move |key| match key {
            LAUNCH_NONCE_FD_ENV => Some(OsString::from(value.clone())),
            // Present so a test sees any fallback to the environment copy.
            LAUNCH_NONCE_ENV => Some(OsString::from("env-copy")),
            _ => None,
        }
    }

    #[test]
    fn two_readers_share_one_descriptor_read() {
        let _serial = serial();
        let (read_end, inode) = sealed_pipe(NONCE.as_bytes());
        let fd = read_end.into_raw_fd();
        let cell = Arc::new(LaunchNonceCell::new());

        // Many first callers at once: one reads, the others wait for it.
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let cell = Arc::clone(&cell);
                std::thread::spawn(move || cell.get(naming(format!("{fd}:{inode}"))))
            })
            .collect();
        for thread in threads {
            let got = thread.join().unwrap().unwrap().unwrap();
            assert_eq!(got.value(), NONCE);
            assert_eq!(got.source(), LaunchNonceSource::Fd);
        }
        assert_eq!(cell.descriptor_reads(), 1);
        assert!(!fd_is_open(fd), "the first read closes the descriptor");

        // Put a different pipe at the same number and name it, inode and
        // all, the way a second independent reader would find it. A reader
        // that went back to the descriptor would now take the planted bytes;
        // the cached value must come back instead and leave them unread.
        // F_DUPFD returns the lowest free number at or above `fd`, so it
        // lands exactly on `fd` only while that number is free, without ever
        // replacing a descriptor another thread has just opened there.
        let (planted, planted_inode) = sealed_pipe(b"planted");
        // Being the lowest free number, the new pipe usually lands on `fd`
        // by itself.
        let (planted, mut placed) = if planted.as_raw_fd() == fd {
            (None, planted.into_raw_fd())
        } else {
            (Some(planted), -1)
        };
        for _ in 0..100 {
            let Some(planted) = &planted else { break };
            // SAFETY: duplicates a descriptor this test owns.
            #[allow(unsafe_code)]
            let copy = unsafe { libc::fcntl(planted.as_raw_fd(), libc::F_DUPFD_CLOEXEC, fd) };
            if copy == fd {
                placed = copy;
                break;
            }
            // SAFETY: closes the copy just made, which this test owns.
            #[allow(unsafe_code)]
            unsafe {
                libc::close(copy)
            };
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(placed, fd, "descriptor {fd} stayed taken by someone else");
        // SAFETY: `fd` now holds this test's copy of the planted pipe.
        #[allow(unsafe_code)]
        let placed = unsafe { OwnedFd::from_raw_fd(placed) };

        let second = cell
            .get(naming(format!("{fd}:{planted_inode}")))
            .unwrap()
            .unwrap();
        assert_eq!(second.value(), NONCE);
        assert_eq!(cell.descriptor_reads(), 1);
        drop(planted);
        let mut still_there = String::new();
        File::from(placed).read_to_string(&mut still_there).unwrap();
        assert_eq!(still_there, "planted");
    }

    #[test]
    fn a_closed_descriptor_is_not_open_and_never_falls_back() {
        let _serial = serial();
        // A number far above anything this test binary opens.
        let fd: RawFd = 900;
        assert!(!fd_is_open(fd));
        let got = LaunchNonceCell::new().get(naming(format!("{fd}:1")));
        assert!(
            matches!(got, Err(LaunchNonceError::NotOpen { fd: 900, .. })),
            "{got:?}"
        );
    }

    #[test]
    fn a_descriptor_that_is_not_a_pipe_is_left_open() {
        let _serial = serial();
        let file = File::open("/dev/null").unwrap();
        let fd = file.as_raw_fd();
        let cell = LaunchNonceCell::new();
        let got = cell.get(naming(format!("{fd}:1")));
        assert_eq!(got, Err(LaunchNonceError::NotAPipe { fd }));
        assert!(
            fd_is_open(fd),
            "the accessor must not close a file it does not own"
        );
        assert_eq!(cell.descriptor_reads(), 0);
    }

    #[test]
    fn a_different_pipe_at_the_number_is_left_unread() {
        let _serial = serial();
        let (_ours, expected) = sealed_pipe(NONCE.as_bytes());
        let (theirs, found) = sealed_pipe(b"someone else's");
        assert_ne!(expected, found, "pipes need distinct inodes for this test");
        let fd = theirs.as_raw_fd();
        let cell = LaunchNonceCell::new();
        let got = cell.get(naming(format!("{fd}:{expected}")));
        assert_eq!(
            got,
            Err(LaunchNonceError::WrongPipe {
                fd,
                expected_inode: expected,
                found_inode: found
            })
        );
        assert_eq!(cell.descriptor_reads(), 0);
        let mut left = String::new();
        File::from(theirs).read_to_string(&mut left).unwrap();
        assert_eq!(left, "someone else's");
    }

    #[test]
    fn an_empty_pipe_is_its_own_error_and_is_left_open() {
        let _serial = serial();
        let (empty, inode) = sealed_pipe(b"");
        let fd = empty.as_raw_fd();
        let cell = LaunchNonceCell::new();
        let got = cell.get(naming(format!("{fd}:{inode}")));
        assert_eq!(got, Err(LaunchNonceError::Empty { fd }));
        assert_eq!(cell.descriptor_reads(), 0);
        assert!(fd_is_open(fd), "an empty pipe is refused, not consumed");
    }

    #[test]
    fn the_named_pipe_is_read_and_closed() {
        let _serial = serial();
        let (ours, inode) = sealed_pipe(NONCE.as_bytes());
        let fd = ours.into_raw_fd();
        let got = LaunchNonceCell::new()
            .get(naming(format!("{fd}:{inode}")))
            .unwrap()
            .unwrap();
        assert_eq!(
            (got.value(), got.source()),
            (NONCE, LaunchNonceSource::Fd)
        );
        assert!(!fd_is_open(fd));
    }

    #[test]
    fn malformed_values_are_refused() {
        let _serial = serial();
        for value in ["", "3", "x:1", "-1:5", "3:", "3:x", ":5"] {
            let got = LaunchNonceCell::new().get(naming(value.to_string()));
            assert!(
                matches!(got, Err(LaunchNonceError::Malformed { .. })),
                "{value}: {got:?}"
            );
        }
    }

    // Counts allocations made on the current thread, so the pre-exec step
    // can be shown to make none while other tests allocate on their own.
    struct CountingAllocator;
    thread_local! {
        static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
    }
    #[allow(unsafe_code)]
    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
            // SAFETY: forwards the caller's layout to the system allocator.
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: `ptr` came from `alloc` above with this layout.
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
            // SAFETY: as for `alloc` and `dealloc`.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }
    #[global_allocator]
    static ALLOCATOR: CountingAllocator = CountingAllocator;

    fn allocations() -> usize {
        ALLOCATIONS.with(Cell::get)
    }

    #[test]
    fn the_pre_exec_step_does_not_allocate() {
        let _serial = serial();
        // Run the step in this process onto a free high number, then on the
        // already-in-place path and on the error path.
        let moved = LaunchNonceHandoff::new(NONCE).unwrap().with_target(250);
        let in_place = LaunchNonceHandoff::new(NONCE).unwrap();
        let in_place_number = in_place.read_end_fd();
        let in_place = in_place.with_target(in_place_number);
        let failing = LaunchNonceHandoff::new(NONCE).unwrap().with_target(-5);

        let before = allocations();
        let moved_result = moved.install_in_child();
        let in_place_result = in_place.install_in_child();
        let failing_result = failing.install_in_child();
        let after = allocations();

        assert!(moved_result.is_ok(), "{moved_result:?}");
        assert!(in_place_result.is_ok(), "{in_place_result:?}");
        assert!(failing_result.is_err());
        assert_eq!(after - before, 0, "the pre-exec step allocated");
        // SAFETY: F_GETFD reads flags of descriptors this test created.
        #[allow(unsafe_code)]
        let (moved_flags, in_place_flags) = unsafe {
            (
                libc::fcntl(250, libc::F_GETFD),
                libc::fcntl(in_place_number, libc::F_GETFD),
            )
        };
        assert_eq!(moved_flags & libc::FD_CLOEXEC, 0, "the copy must survive exec");
        assert_eq!(in_place_flags & libc::FD_CLOEXEC, 0, "the flag must be cleared in place");
        // SAFETY: closes the copy this test made at 250.
        #[allow(unsafe_code)]
        unsafe {
            libc::close(250)
        };
    }

    #[test]
    fn a_new_handoff_is_close_on_exec_in_the_parent() {
        let _serial = serial();
        let handoff = LaunchNonceHandoff::new(NONCE).unwrap();
        // SAFETY: F_GETFD reads flags.
        #[allow(unsafe_code)]
        let flags = unsafe { libc::fcntl(handoff.read_end_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        assert_eq!(
            handoff.fd_env_value(),
            format!("3:{}", handoff.inode())
        );
    }

    // ---- Spawned processes -------------------------------------------------
    //
    // The tests below run this test binary again as a child, selecting the
    // ignored `probe_child` test, so the child is a real process that received
    // the handoff through exec. It writes what it saw to a report file.

    const ROLE_ENV: &str = "SUBC_OS_TEST_PROBE_ROLE";
    const OUT_ENV: &str = "SUBC_OS_TEST_PROBE_OUT";

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "subc-os-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A command running `probe_child` in `role`, reporting to `out`. The
    /// child's XDG directories point into the scratch directory so nothing it
    /// could start touches the user's real ones.
    fn probe_command(out: &Path, role: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "launch_nonce::tests::unix_tests::probe_child",
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
                "-q",
            ])
            .env(ROLE_ENV, role)
            .env(OUT_ENV, out)
            .stdout(Stdio::null());
        let xdg = out.parent().unwrap();
        for name in ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"] {
            command.env(name, xdg.join(name));
        }
        command
    }

    fn describe(result: &Result<Option<super::super::LaunchNonce>, LaunchNonceError>) -> String {
        match result {
            Ok(Some(nonce)) => format!("{}:{}", nonce.source().as_str(), nonce.value()),
            Ok(None) => "none".to_string(),
            Err(error) => {
                let debug = format!("{error:?}");
                let name = debug.split([' ', '{', '(']).next().unwrap_or_default();
                format!("err:{name}")
            }
        }
    }

    fn report_line<'a>(report: &'a str, key: &str) -> &'a str {
        report
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| panic!("no {key} in report:\n{report}"))
    }

    fn run(mut command: Command, out: &Path) -> String {
        let status = command.status().unwrap();
        assert!(status.success(), "probe child failed: {status}");
        fs::read_to_string(out).unwrap()
    }

    #[test]
    #[ignore = "run as a child process by the spawned-process tests"]
    fn probe_child() {
        let Some(role) = std::env::var(ROLE_ENV).ok() else {
            return;
        };
        let out = PathBuf::from(std::env::var_os(OUT_ENV).unwrap());
        let mut report = String::new();
        if let Some(fd) = role.strip_prefix("cat-fd:") {
            // Read a descriptor raw, to show what the step left at a number.
            let fd: RawFd = fd.parse().unwrap();
            // SAFETY: the parent put this descriptor here for this read.
            #[allow(unsafe_code)]
            let mut file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
            let mut bytes = String::new();
            let read = file.read_to_string(&mut bytes);
            report.push_str(&format!("read={}\n", read.is_ok()));
            report.push_str(&format!("bytes={bytes}\n"));
            fs::write(&out, report).unwrap();
            return;
        }
        let before: Vec<_> = std::env::vars_os().collect();
        let first = launch_nonce();
        let second = launch_nonce();
        let after: Vec<_> = std::env::vars_os().collect();
        report.push_str(&format!("result={}\n", describe(&first)));
        report.push_str(&format!("second_equal={}\n", first == second));
        report.push_str(&format!("env_unchanged={}\n", before == after));
        if role == "module-with-grandchild" {
            // Spawned after the first read, with the environment this process
            // inherited: the descriptor variable and the environment copy.
            let grandchild_out = out.with_extension("grandchild");
            let status = probe_command(&grandchild_out, "grandchild")
                .status()
                .unwrap();
            let grandchild = fs::read_to_string(&grandchild_out).unwrap_or_default();
            report.push_str(&format!("grandchild_ok={}\n", status.success()));
            report.push_str(&format!(
                "grandchild_result={}\n",
                report_line(&grandchild, "result")
            ));
        }
        fs::write(&out, report).unwrap();
    }

    #[test]
    fn a_spawned_child_reads_the_nonce_from_descriptor_3_and_leaves_the_environment_alone() {
        let _serial = serial();
        let scratch = Scratch::new("fd-child");
        let out = scratch.0.join("report");
        let mut command = probe_command(&out, "module");
        let handoff = LaunchNonceHandoff::new(NONCE).unwrap();
        command.env(LAUNCH_NONCE_FD_ENV, handoff.fd_env_value());
        // A different environment copy, so the report shows which was read.
        command.env(LAUNCH_NONCE_ENV, "environment-copy-is-not-the-nonce");
        handoff.install_last(&mut command);
        let report = run(command, &out);
        assert_eq!(report_line(&report, "result"), format!("fd:{NONCE}"));
        assert_eq!(report_line(&report, "second_equal"), "true");
        assert_eq!(report_line(&report, "env_unchanged"), "true");
    }

    #[test]
    fn a_spawned_child_without_a_descriptor_reads_the_environment_copy() {
        let _serial = serial();
        let scratch = Scratch::new("env-child");
        let out = scratch.0.join("report");
        let mut command = probe_command(&out, "module");
        command.env_remove(LAUNCH_NONCE_FD_ENV);
        command.env(LAUNCH_NONCE_ENV, NONCE);
        let report = run(command, &out);
        assert_eq!(report_line(&report, "result"), format!("env:{NONCE}"));
        assert_eq!(report_line(&report, "env_unchanged"), "true");
    }

    #[test]
    fn a_grandchild_spawned_after_the_first_read_gets_no_descriptor_and_refuses_by_name() {
        let _serial = serial();
        let scratch = Scratch::new("grandchild");
        let out = scratch.0.join("report");
        let mut command = probe_command(&out, "module-with-grandchild");
        let handoff = LaunchNonceHandoff::new(NONCE).unwrap();
        command.env(LAUNCH_NONCE_FD_ENV, handoff.fd_env_value());
        command.env(LAUNCH_NONCE_ENV, NONCE);
        handoff.install_last(&mut command);
        let report = run(command, &out);
        assert_eq!(report_line(&report, "result"), format!("fd:{NONCE}"));
        assert_eq!(report_line(&report, "grandchild_ok"), "true");
        let grandchild = report_line(&report, "grandchild_result");
        // The grandchild inherited both variables. It must refuse by name,
        // and must not fall back to the environment copy it can still see.
        assert!(
            ["err:NotOpen", "err:NotAPipe", "err:WrongPipe"].contains(&grandchild),
            "grandchild got {grandchild}"
        );
    }

    #[test]
    fn the_handoff_installed_after_an_earlier_step_survives_a_colliding_descriptor_3() {
        let _serial = serial();
        let scratch = Scratch::new("collision");
        let out = scratch.0.join("report");
        let marker_path = scratch.0.join("marker");
        let marker = File::create(&marker_path).unwrap();
        let marker_fd = marker.as_raw_fd();
        let mut command = probe_command(&out, "cat-fd:3");
        // Stands in for the Linux cgroup placement: an earlier step that
        // writes through a descriptor it captured, which here has number 3.
        // SAFETY: dup2 and write only, on descriptors opened before the fork.
        #[allow(unsafe_code)]
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(marker_fd, 3) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                let line = b"earlier step\n";
                if libc::write(3, line.as_ptr().cast(), line.len()) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        LaunchNonceHandoff::new(NONCE)
            .unwrap()
            .install_last(&mut command);
        let report = run(command, &out);
        drop(marker);
        assert_eq!(report_line(&report, "bytes"), NONCE);
        assert_eq!(fs::read_to_string(&marker_path).unwrap(), "earlier step\n");
    }

    #[test]
    fn a_read_end_already_at_the_target_number_still_reaches_the_child() {
        let _serial = serial();
        let scratch = Scratch::new("in-place");
        let out = scratch.0.join("report");
        let handoff = LaunchNonceHandoff::new(NONCE).unwrap();
        let number = handoff.read_end_fd();
        let handoff = handoff.with_target(number);
        let mut command = probe_command(&out, &format!("cat-fd:{number}"));
        handoff.install_last(&mut command);
        let report = run(command, &out);
        assert_eq!(report_line(&report, "read"), "true");
        assert_eq!(report_line(&report, "bytes"), NONCE);
    }

    #[test]
    fn the_accessor_leaves_this_process_environment_alone() {
        let _serial = serial();
        let before: Vec<_> = std::env::vars_os().collect();
        let (ours, inode) = sealed_pipe(NONCE.as_bytes());
        let fd = ours.into_raw_fd();
        LaunchNonceCell::new()
            .get(naming(format!("{fd}:{inode}")))
            .unwrap()
            .unwrap();
        let after: Vec<_> = std::env::vars_os().collect();
        assert_eq!(before, after);
    }
}
