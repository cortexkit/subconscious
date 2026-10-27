//! Each case observes a separate child process (this test binary re-run as a
//! fixture) so that an implementation describing the observing process
//! itself, rather than the peer, fails.

use super::*;
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// Kills and reaps a fixture child however its test ends.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_fixture(test: &str, env: &str, value: &std::ffi::OsStr) -> ChildGuard {
    ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args([
                test,
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(env, value)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

fn same_file(left: &Path, right: &Path) -> bool {
    left.canonicalize().unwrap() == right.canonicalize().unwrap()
}

fn observation(peer: ObservedPeer) -> PeerObservation {
    match peer {
        ObservedPeer::Observed(observation) => observation,
        ObservedPeer::Unobserved => panic!("expected an observed peer"),
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use cortexkit_test_support::ScratchDir;
    #[cfg(target_os = "linux")]
    use std::os::fd::AsFd;
    use std::{
        io::Read,
        os::unix::net::{UnixListener, UnixStream},
        path::PathBuf,
    };

    const CONNECT_ENV: &str = "SUBC_OS_LOCAL_PEER_CONNECT";
    /// A socket path short enough for `sockaddr_un` (104 bytes on macOS).
    /// The per-process scratch directory can be longer than that on macOS, so
    /// fall back to a uniquely named file in the system temp directory.
    struct SocketPath {
        path: PathBuf,
        _scratch: ScratchDir,
    }

    impl SocketPath {
        fn new() -> Self {
            let scratch = ScratchDir::new("subc-lp");
            let mut path = scratch.join("s");
            if path.as_os_str().len() >= 100 {
                use std::sync::atomic::{AtomicU32, Ordering};
                static NEXT: AtomicU32 = AtomicU32::new(0);
                path = std::env::temp_dir().join(format!(
                    "subc-lp-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                let _ = std::fs::remove_file(&path);
            }
            Self {
                path,
                _scratch: scratch,
            }
        }
    }

    impl Drop for SocketPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    struct Connection {
        stream: UnixStream,
        child: ChildGuard,
        _path: SocketPath,
    }

    /// Accept one connection from `child`, giving up if it exits or takes
    /// longer than ten seconds.
    fn accept_from(path: SocketPath, listener: UnixListener, mut child: ChildGuard) -> Connection {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    return Connection {
                        stream,
                        child,
                        _path: path,
                    };
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("accept: {error}"),
            }
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "fixture exited without connecting"
            );
            assert!(Instant::now() < deadline, "fixture never connected");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn connect_fixture() -> Connection {
        let path = SocketPath::new();
        let listener = UnixListener::bind(&path.path).unwrap();
        let child = spawn_fixture(
            "local_peer::tests::unix::connecting_child",
            CONNECT_ENV,
            path.path.as_os_str(),
        );
        accept_from(path, listener, child)
    }

    #[test]
    #[ignore = "re-exec fixture that connects and holds the socket until its parent closes stdin"]
    fn connecting_child() {
        let path = std::env::var_os(CONNECT_ENV).unwrap();
        let stream = UnixStream::connect(path).unwrap();
        let _ = std::io::stdin().read(&mut [0]);
        drop(stream);
    }

    #[cfg(target_os = "linux")]
    fn kernel_has_peer_pidfd() -> bool {
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap();
        let mut parts = release
            .trim()
            .split(|c: char| !c.is_ascii_digit())
            .map(|part| part.parse::<u32>().unwrap_or(0));
        let version = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
        version >= (6, 5)
    }

    /// `codesign` reports the identifier sealed into a file's signature and
    /// whether the signature verifies, independently of the code under test.
    #[cfg(target_os = "macos")]
    fn codesign_identifier(exe: &Path) -> Option<String> {
        let verified = Command::new("/usr/bin/codesign")
            .arg("--verify")
            .arg(exe)
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        if !verified {
            return None;
        }
        let output = Command::new("/usr/bin/codesign")
            .args(["-d", "-v"])
            .arg(exe)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .find_map(|line| line.strip_prefix("Identifier=").map(str::to_owned))
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn unix_peer_is_the_connecting_process() {
        let connection = connect_fixture();
        let observed = observe_unix_peer(&connection.stream);
        #[cfg(target_os = "linux")]
        if !kernel_has_peer_pidfd() {
            assert_eq!(observed, ObservedPeer::Unobserved);
            eprintln!("kernel lacks SO_PEERPIDFD; checked the unobserved fallback only");
            return;
        }
        let observed = observation(observed);
        assert_eq!(observed.pid, connection.child.0.id());
        assert_ne!(observed.pid, std::process::id());
        assert_eq!(observed.user, PeerUser::Uid(effective_uid()));
        let exe = observed.exe.as_deref().expect("peer executable");
        assert!(same_file(exe, &std::env::current_exe().unwrap()), "{exe:?}");
        #[cfg(target_os = "macos")]
        assert_eq!(observed.signing_id, codesign_identifier(exe));
        #[cfg(not(target_os = "macos"))]
        assert_eq!(observed.signing_id, None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[allow(unsafe_code)]
    fn effective_uid() -> u32 {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }

    /// A system binary signed by Apple, found by its audit token: the guest
    /// code and signing identifier belong to `nc`, not to this test binary.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_peer_signing_id_comes_from_the_peer_code() {
        let nc = Path::new("/usr/bin/nc");
        let path = SocketPath::new();
        let listener = UnixListener::bind(&path.path).unwrap();
        let child = ChildGuard(
            Command::new(nc)
                .arg("-U")
                .arg(&path.path)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let connection = accept_from(path, listener, child);
        let observed = observation(observe_unix_peer(&connection.stream));
        assert_eq!(observed.pid, connection.child.0.id());
        let exe = observed.exe.as_deref().expect("peer executable");
        assert!(same_file(exe, nc), "{exe:?}");
        let expected = codesign_identifier(nc);
        assert!(expected.is_some(), "system nc verifies");
        assert_eq!(observed.signing_id, expected);
    }

    #[test]
    fn forced_user_failure_is_unobserved() {
        let connection = connect_fixture();
        let forced = ForcedFailures {
            user: true,
            ..ForcedFailures::default()
        };
        assert_eq!(
            observe_unix_peer_forcing(&connection.stream, forced),
            ObservedPeer::Unobserved
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn forced_exe_failure_omits_only_exe() {
        let connection = connect_fixture();
        let full = observe_unix_peer(&connection.stream);
        #[cfg(target_os = "linux")]
        if !kernel_has_peer_pidfd() {
            return;
        }
        let full = observation(full);
        assert!(full.exe.is_some());
        let forced = ForcedFailures {
            exe: true,
            ..ForcedFailures::default()
        };
        let without_exe = observation(observe_unix_peer_forcing(&connection.stream, forced));
        assert_eq!(
            without_exe,
            PeerObservation {
                exe: None,
                ..full.clone()
            }
        );
    }

    /// Forced, because a live peer with a broken signature could not be
    /// arranged as a fixture on Apple silicon: a process that marks its own
    /// code invalid (`csops`) is killed by the kernel before it can be
    /// observed, and a modified file whose changed page never loads still
    /// passes `SecCodeCheckValidity`, which checks the running code.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_forced_validity_failure_omits_only_signing_id() {
        let connection = connect_fixture();
        let full = observation(observe_unix_peer(&connection.stream));
        let forced = ForcedFailures {
            signing_id: true,
            ..ForcedFailures::default()
        };
        let invalid = observation(observe_unix_peer_forcing(&connection.stream, forced));
        assert!(invalid.exe.is_some());
        assert_eq!(
            invalid,
            PeerObservation {
                signing_id: None,
                ..full
            }
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_without_peer_pidfd_is_unobserved() {
        let connection = connect_fixture();
        let forced = ForcedFailures {
            peer_pidfd: true,
            ..ForcedFailures::default()
        };
        assert_eq!(
            observe_unix_peer_forcing(&connection.stream, forced),
            ObservedPeer::Unobserved
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_exe_is_dropped_when_the_pidfd_reports_exit_after_the_read() {
        if !kernel_has_peer_pidfd() {
            return;
        }
        let connection = connect_fixture();
        let observed = observation(super::super::linux::observe_checking(
            connection.stream.as_fd(),
            ForcedFailures::default(),
            |_| false,
        ));
        assert_eq!(observed.pid, connection.child.0.id());
        assert_eq!(observed.exe, None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_pidfd_alive_tracks_process_exit() {
        use rustix::process::{pidfd_open, Pid, PidfdFlags};

        let mut child = ChildGuard(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "local_peer::tests::unix::connecting_child",
                    "--exact",
                    "--ignored",
                ])
                .env(CONNECT_ENV, "/nonexistent/subc-os-local-peer")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        // The child fails to connect and exits; until reaped, its pid stays
        // valid for pidfd_open.
        let pid = Pid::from_raw(child.0.id() as i32).unwrap();
        let pidfd = pidfd_open(pid, PidfdFlags::empty()).unwrap();
        child.0.wait().unwrap();
        assert!(!super::super::linux::pidfd_alive(&pidfd));

        let live = connect_fixture();
        let pid = Pid::from_raw(live.child.0.id() as i32).unwrap();
        let pidfd = pidfd_open(pid, PidfdFlags::empty()).unwrap();
        assert!(super::super::linux::pidfd_alive(&pidfd));
    }
}

#[cfg(windows)]
mod windows {
    #![allow(unsafe_code)]

    use super::*;
    use std::{
        io::Read,
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        },
        sync::mpsc,
    };
    use windows_sys::Win32::{
        Foundation::{ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE},
        Storage::FileSystem::PIPE_ACCESS_DUPLEX,
        System::Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
            PIPE_WAIT,
        },
    };

    const PIPE_ENV: &str = "SUBC_OS_LOCAL_PEER_PIPE";

    fn create_server(name: &str) -> OwnedHandle {
        let wide: Vec<u16> = std::ffi::OsStr::new(name)
            .encode_wide()
            .chain(Some(0))
            .collect();
        // SAFETY: wide is NUL-terminated and live for the call; default
        // security is enough for a same-user test client.
        let handle = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                0,
                0,
                0,
                std::ptr::null(),
            )
        };
        assert!(
            !handle.is_null() && handle != INVALID_HANDLE_VALUE,
            "CreateNamedPipeW: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: a new server handle owned here alone.
        unsafe { OwnedHandle::from_raw_handle(handle) }
    }

    struct Connection {
        server: OwnedHandle,
        child: ChildGuard,
    }

    fn connect_fixture() -> Connection {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let name = format!(
            r"\\.\pipe\subc-os-local-peer-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let server = create_server(&name);
        let mut child = spawn_fixture(
            "local_peer::tests::windows::connecting_child",
            PIPE_ENV,
            std::ffi::OsStr::new(&name),
        );
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            // SAFETY: server is a live, unconnected pipe instance; a null
            // OVERLAPPED makes the call block until a client connects.
            let connected =
                unsafe { ConnectNamedPipe(server.as_raw_handle(), std::ptr::null_mut()) } != 0
                    || std::io::Error::last_os_error().raw_os_error()
                        == Some(ERROR_PIPE_CONNECTED as i32);
            let _ = sender.send(connected.then_some(server));
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(server) => {
                    return Connection {
                        server: server.expect("ConnectNamedPipe"),
                        child,
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(error) => panic!("{error}"),
            }
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "fixture exited without connecting"
            );
            assert!(Instant::now() < deadline, "fixture never connected");
        }
    }

    #[test]
    #[ignore = "re-exec fixture that opens the pipe and holds it until its parent closes stdin"]
    fn connecting_child() {
        let name = std::env::var_os(PIPE_ENV).unwrap();
        let pipe = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name)
            .unwrap();
        let _ = std::io::stdin().read(&mut [0]);
        drop(pipe);
    }

    /// The current user's SID as `whoami` prints it, independent of the token
    /// reads under test.
    fn whoami_sid() -> String {
        let output = Command::new("whoami")
            .args(["/user", "/fo", "csv", "/nh"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let text = String::from_utf8_lossy(&output.stdout);
        text.trim()
            .rsplit(',')
            .next()
            .unwrap()
            .trim_matches('"')
            .to_owned()
    }

    #[test]
    fn pipe_client_is_the_connecting_process() {
        let connection = connect_fixture();
        let observed = observation(observe_pipe_client(&connection.server));
        assert_eq!(observed.pid, connection.child.0.id());
        assert_ne!(observed.pid, std::process::id());
        assert_eq!(observed.user, PeerUser::Sid(whoami_sid()));
        let exe = observed.exe.as_deref().expect("client executable");
        assert!(same_file(exe, &std::env::current_exe().unwrap()), "{exe:?}");
        assert_eq!(observed.signing_id, None);
    }

    #[test]
    fn forced_sid_failure_is_unobserved() {
        let connection = connect_fixture();
        let forced = ForcedFailures {
            user: true,
            ..ForcedFailures::default()
        };
        assert_eq!(
            observe_pipe_client_forcing(&connection.server, forced),
            ObservedPeer::Unobserved
        );
    }

    #[test]
    fn forced_exe_failure_omits_only_exe() {
        let connection = connect_fixture();
        let full = observation(observe_pipe_client(&connection.server));
        assert!(full.exe.is_some());
        let forced = ForcedFailures {
            exe: true,
            ..ForcedFailures::default()
        };
        let without_exe = observation(observe_pipe_client_forcing(&connection.server, forced));
        assert_eq!(without_exe, PeerObservation { exe: None, ..full });
    }
}
