//! macOS responsibility isolation at exec. macOS charges privacy permissions
//! (Screen Recording, Accessibility, Files and Folders) to a process's
//! "responsible process", which is normally the program that launched it, so a
//! supervised module would otherwise borrow the daemon's grants. The daemon
//! therefore launches each module through a trampoline: `ck-subc` re-executed
//! with a hidden first argument. The trampoline calls `posix_spawnp` with
//! `POSIX_SPAWN_SETEXEC`, a Darwin flag that replaces the calling process's
//! image in place (like `execve`) instead of creating a child, after marking the
//! spawn as disclaiming responsibility. The module keeps the trampoline's pid,
//! process group, pipes and fd-3 launch-nonce descriptor, and becomes its own
//! responsible process.

/// Reserved exit statuses from the trampoline, distinct from ordinary CLI errors.
pub fn failure_cause(code: Option<i32>) -> Option<&'static str> {
    match code {
        Some(120) => Some("responsibility_spawnattrs_setdisclaim is unavailable"),
        Some(121) => Some("responsibility_spawnattrs_setdisclaim failed"),
        Some(122) => Some("privacy identity posix_spawn failed"),
        Some(123) => Some("privacy identity trampoline setup failed"),
        _ => None,
    }
}

/// Capability response checked by embedding supervisors before using a binary.
pub const TRAMPOLINE_PROBE: &str = "subc-privacy-trampoline/v1";
/// A refused SETEXEC writes this tag and its cause before closing the ack pipe.
/// Empty EOF means exec succeeded, regardless of the module's eventual status.
pub const EXEC_REFUSAL_TAG: &str = "SUBC_PRIVACY_REFUSAL_V1 ";

/// Handle the hidden first argument before building any runtime. Returns only
/// for ordinary daemon arguments. Every hidden-mode error exits, never execs a
/// module with inherited responsibility.
pub fn trampoline_main(args: &[std::ffi::OsString]) {
    trampoline_entry(args, false);
}

#[cfg(feature = "test-support")]
pub fn trampoline_main_for_test(args: &[std::ffi::OsString]) {
    trampoline_entry(args, true);
}

fn trampoline_entry(args: &[std::ffi::OsString], test_hooks: bool) {
    if !args.first().is_some_and(|arg| arg == "__disclaim-exec") {
        return;
    }
    #[cfg(target_os = "macos")]
    {
        if args.len() == 2 && args[1] == "--probe" {
            #[cfg(feature = "test-support")]
            let missing =
                test_hooks && std::env::var_os("SUBC_TEST_PRIVACY_MISSING_SYMBOL").is_some();
            #[cfg(not(feature = "test-support"))]
            let missing = false;
            match macos::resolve_disclaim(missing) {
                Ok(_) => {
                    println!("{TRAMPOLINE_PROBE}");
                    std::process::exit(0);
                }
                Err(error) => {
                    eprintln!("ck-subc: own privacy identity refused: {error}");
                    std::process::exit(error.exit_code());
                }
            }
        }
        #[cfg(feature = "test-support")]
        if test_hooks {
            let error = macos::disclaim_exec_for_test(&args[1..])
                .expect_err("SETEXEC cannot return on success");
            eprintln!("ck-subc: own privacy identity refused: {error}");
            std::process::exit(error.exit_code());
        }
        let _ = test_hooks;
        let error = macos::disclaim_exec(&args[1..]).expect_err("SETEXEC cannot return on success");
        eprintln!("ck-subc: own privacy identity refused: {error}");
        std::process::exit(error.exit_code());
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = test_hooks;
        eprintln!("ck-subc: own privacy identity is only available on macOS");
        std::process::exit(123);
    }
}

#[cfg(target_os = "macos")]
pub use macos::{disclaim_exec, ExecAcknowledgement, ExecError};

#[cfg(all(target_os = "macos", feature = "test-support"))]
pub use macos::{disclaim_exec_for_test, privacy_observation_for_test};

#[cfg(target_os = "macos")]
mod macos {
    use std::{
        ffi::{CString, OsString},
        fmt, io,
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::ffi::OsStrExt,
        },
    };

    #[derive(Debug)]
    pub struct ExecError {
        code: i32,
        detail: String,
    }

    impl ExecError {
        pub fn exit_code(&self) -> i32 {
            self.code
        }
        fn new(code: i32, detail: impl fmt::Display) -> Self {
            Self {
                code,
                detail: detail.to_string(),
            }
        }
    }

    impl fmt::Display for ExecError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(
                f,
                "{}: {}",
                super::failure_cause(Some(self.code)).unwrap_or("privacy identity failure"),
                self.detail
            )
        }
    }

    impl std::error::Error for ExecError {}

    /// A second pipe, independent of the fd-3 launch-nonce pipe. The trampoline
    /// marks its write end close-on-exec, so EOF means the trampoline either
    /// replaced itself with the module or exited.
    pub struct ExecAcknowledgement {
        writer: OwnedFd,
    }

    impl ExecAcknowledgement {
        pub fn pipe() -> io::Result<(std::io::PipeReader, Self)> {
            let (reader, writer) = io::pipe()?;
            // SAFETY: nonblocking reads let the daemon await EOF with AsyncFd
            // rather than block a runtime worker while the trampoline starts.
            #[allow(unsafe_code)]
            unsafe {
                let flags = libc::fcntl(reader.as_raw_fd(), libc::F_GETFL);
                if flags == -1
                    || libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK)
                        == -1
                {
                    return Err(io::Error::last_os_error());
                }
            }
            // SAFETY: duplicates the owned descriptor to number 4 or higher, so it
            // cannot collide with stdio (0-2) or the launch-nonce descriptor the
            // child receives as fd 3. The parent copy stays CLOEXEC; only our
            // child clears that flag.
            #[allow(unsafe_code)]
            let fd = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 4) };
            if fd == -1 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful fcntl returned a fresh descriptor owned here.
            #[allow(unsafe_code)]
            let writer = unsafe { OwnedFd::from_raw_fd(fd) };
            Ok((reader, Self { writer }))
        }

        pub fn fd(&self) -> i32 {
            self.writer.as_raw_fd()
        }

        /// Register before the final nonce handoff. Retain `self` until spawn
        /// returns, then drop it and the Command so the parent holds no writer.
        pub fn install(&self, command: &mut std::process::Command) {
            use std::os::unix::process::CommandExt;
            let fd = self.fd();
            // SAFETY: pre_exec makes only fcntl calls, allocates nothing, and
            // touches no Rust locks. The captured descriptor is kept alive by
            // the caller until spawn completes and is never fd 3 or stdio.
            #[allow(unsafe_code)]
            unsafe {
                command.pre_exec(move || {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags == -1
                        || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1
                    {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
    }

    /// Execute `<ack fd> <program> <args...>` as a new responsible process.
    /// Success never returns. This must run in a freshly exec'd, single-threaded
    /// trampoline, NOT in pre_exec: CString and environment construction allocate.
    pub fn disclaim_exec(args: &[OsString]) -> Result<(), ExecError> {
        let result = exec(args, false);
        if let Err(error) = &result {
            write_refusal_record(args, error);
        }
        result
    }

    /// Only the dedicated fixture binary calls this. No production config or
    /// environment variable can select another lookup symbol in `ck-subc`.
    #[cfg(feature = "test-support")]
    pub fn disclaim_exec_for_test(args: &[OsString]) -> Result<(), ExecError> {
        if std::env::var_os("SUBC_TEST_PRIVACY_CLOSE_ACK_WITHOUT_EXEC").is_some() {
            let fd: i32 = args[0].to_str().unwrap().parse().unwrap();
            // SAFETY: the fixture was handed this descriptor by its supervisor;
            // closing it simulates a premature exec acknowledgement, not exec.
            #[allow(unsafe_code)]
            unsafe {
                libc::close(fd);
            }
            std::thread::sleep(std::time::Duration::from_secs(6));
            return Err(ExecError::new(123, "test trampoline did not exec"));
        }
        if let Ok(ms) = std::env::var("SUBC_TEST_PRIVACY_EXEC_DELAY_MS") {
            std::thread::sleep(std::time::Duration::from_millis(ms.parse().unwrap()));
        }
        let result = exec(
            args,
            std::env::var_os("SUBC_TEST_PRIVACY_MISSING_SYMBOL").is_some(),
        );
        if let Err(error) = &result {
            write_refusal_record(args, error);
        }
        result
    }

    fn write_refusal_record(args: &[OsString], error: &ExecError) {
        let Some(fd) = args
            .first()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<i32>().ok())
            .filter(|fd| *fd >= 4)
        else {
            return;
        };
        let record = format!("{}{error}\n", super::EXEC_REFUSAL_TAG);
        let bytes = record.as_bytes();
        let mut written = 0;
        while written < bytes.len() {
            // SAFETY: write borrows a live byte buffer and retains no pointer.
            // The descriptor is the inherited ack pipe, never nonce or stdio.
            // This runs after exec in a single-threaded trampoline, not pre_exec.
            #[allow(unsafe_code)]
            let count =
                unsafe { libc::write(fd, bytes[written..].as_ptr().cast(), bytes.len() - written) };
            if count > 0 {
                written += count as usize;
            } else if count == -1 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
            {
                continue;
            } else {
                break;
            }
        }
    }

    type Disclaim = unsafe extern "C" fn(*mut libc::posix_spawnattr_t, libc::c_int) -> libc::c_int;

    pub(super) fn resolve_disclaim(missing_symbol: bool) -> Result<Disclaim, ExecError> {
        let symbol = if missing_symbol {
            c"__subc_test_missing_responsibility_symbol"
        } else {
            c"responsibility_spawnattrs_setdisclaim"
        };
        // SAFETY: RTLD_DEFAULT resolves a fixed NUL-terminated name. The private
        // Darwin ABI is int(posix_spawnattr_t *, int); the function stays loaded
        // for this process's lifetime because it is part of libSystem.
        #[allow(unsafe_code)]
        unsafe {
            let address = libc::dlsym(libc::RTLD_DEFAULT, symbol.as_ptr());
            if address.is_null() {
                return Err(ExecError::new(120, "module was not executed"));
            }
            Ok(std::mem::transmute::<*mut libc::c_void, Disclaim>(address))
        }
    }

    /// The private ABI is confined here. Darwin's SETEXEC preserves the pid,
    /// group and every non-CLOEXEC descriptor; CLOEXEC_DEFAULT must NOT be used.
    /// Every return is an error: there is deliberately no plain-exec fallback.
    fn exec(args: &[OsString], missing_symbol: bool) -> Result<(), ExecError> {
        let disclaim = resolve_disclaim(missing_symbol)?;
        let fd: i32 = args
            .first()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse().ok())
            .filter(|fd| *fd >= 4)
            .ok_or_else(|| ExecError::new(123, "missing exec acknowledgement descriptor"))?;
        if args.len() < 2 {
            return Err(ExecError::new(123, "missing module program"));
        }
        let strings: Vec<CString> = args[1..]
            .iter()
            .map(|s| CString::new(s.as_bytes()))
            .collect::<Result<_, _>>()
            .map_err(|e| ExecError::new(123, e))?;
        let mut argv: Vec<*mut libc::c_char> =
            strings.iter().map(|s| s.as_ptr().cast_mut()).collect();
        argv.push(std::ptr::null_mut());
        let environment: Vec<CString> = std::env::vars_os()
            .map(|(key, value)| {
                let mut bytes = key.as_bytes().to_vec();
                bytes.push(b'=');
                bytes.extend_from_slice(value.as_bytes());
                CString::new(bytes)
            })
            .collect::<Result<_, _>>()
            .map_err(|e| ExecError::new(123, e))?;
        let mut envp: Vec<*mut libc::c_char> =
            environment.iter().map(|s| s.as_ptr().cast_mut()).collect();
        envp.push(std::ptr::null_mut());
        // SAFETY: RTLD_DEFAULT resolves a fixed, NUL-terminated function name.
        // Darwin's ABI is int(posix_spawnattr_t *, int). All argument/env arrays
        // are NUL-terminated and their backing CStrings live through spawn. The
        // initialized attr is destroyed on every returning path. SETEXEC with
        // no file actions retains stdio, fd 3 and the process group in place.
        #[allow(unsafe_code)]
        unsafe {
            // The acknowledgement survives the first exec but closes on SETEXEC.
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags == -1 || libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) == -1 {
                return Err(ExecError::new(123, io::Error::last_os_error()));
            }
            let mut attr = std::mem::MaybeUninit::<libc::posix_spawnattr_t>::uninit();
            let result = libc::posix_spawnattr_init(attr.as_mut_ptr());
            if result != 0 {
                return Err(ExecError::new(123, io::Error::from_raw_os_error(result)));
            }
            let mut attr = attr.assume_init();
            let result = libc::posix_spawnattr_setflags(
                &mut attr,
                libc::POSIX_SPAWN_SETEXEC as libc::c_short,
            );
            let (code, result) = if result != 0 {
                (123, result)
            } else {
                let result = disclaim(&mut attr, 1);
                if result != 0 {
                    (121, result)
                } else {
                    let mut pid = 0;
                    (
                        122,
                        libc::posix_spawnp(
                            &mut pid,
                            strings[0].as_ptr(),
                            std::ptr::null(),
                            &attr,
                            argv.as_ptr(),
                            envp.as_ptr(),
                        ),
                    )
                }
            };
            libc::posix_spawnattr_destroy(&mut attr);
            Err(ExecError::new(
                code,
                if result == 0 {
                    io::Error::other("SETEXEC unexpectedly returned")
                } else {
                    io::Error::from_raw_os_error(result)
                },
            ))
        }
    }

    /// Real-process observation used only by the permission-isolation fixture.
    #[cfg(feature = "test-support")]
    pub fn privacy_observation_for_test() -> io::Result<(u32, u32, u32, u32)> {
        // SAFETY: resolve Darwin's int(pid_t) probe with a fixed name, then pass
        // this process's and its parent's pids. No pointers are retained by the
        // function. getpgrp is an independent kernel observation of containment.
        #[allow(unsafe_code)]
        unsafe {
            let address = libc::dlsym(
                libc::RTLD_DEFAULT,
                c"responsibility_get_pid_responsible_for_pid".as_ptr(),
            );
            if address.is_null() {
                return Err(io::Error::other("responsibility probe unavailable"));
            }
            let probe: unsafe extern "C" fn(libc::pid_t) -> libc::pid_t =
                std::mem::transmute(address);
            Ok((
                probe(libc::getpid()) as u32,
                probe(libc::getppid()) as u32,
                libc::getppid() as u32,
                libc::getpgrp() as u32,
            ))
        }
    }
}
