//! macOS responsibility isolation at exec. macOS charges privacy permissions
//! (Screen Recording, Accessibility, Files and Folders) to a process's
//! "responsible process", which is normally the program that launched it, so a
//! supervised module would otherwise borrow the daemon's grants. The daemon
//! therefore launches each module through a trampoline: `ck-subc` re-executed
//! with a hidden first argument. The trampoline calls `posix_spawnp` with
//! `POSIX_SPAWN_SETEXEC`, a Darwin flag that replaces the calling process's
//! image in place (like `execve`) instead of creating a child, after marking the
//! spawn as disclaiming responsibility. The module keeps the trampoline's pid,
//! process group, pipes and inherited registration descriptors, and becomes its own
//! responsible process.
//!
//! Use [`DisclaimedCommand`] for children of modules as well as supervisors. The
//! trampoline must be the caller's own binary and must handle the hidden mode
//! before starting threads or an async runtime. Call [`probe`] once at startup
//! and refuse to launch children if it fails. On non-macOS platforms the builder
//! launches the program directly, the trampoline path is unused, and both probe
//! and confirmation succeed immediately.
//! Supervised children reserve fd 3 for an ungated nonce, fd 4 for the
//! registration channel and fd 5 for the exec acknowledgement. Acknowledgement
//! sources stay CLOEXEC at fd 6 or above before spawn so stdio cannot overwrite
//! them; the final descriptor placement inherits only slot 5 to the trampoline.
//!
//! Supervised admission uses a separate v2 mode: the daemon probes with
//! `__disclaim-exec --probe-suspended`, then sends
//! `__disclaim-exec-suspended <ack fd> <program> <args...>`. The trampoline
//! answers with a tagged refusal on the ack pipe, or empty EOF when SETEXEC
//! closes it. In v2, SETEXEC also requests START_SUSPENDED: EOF alone is not
//! admission. The daemon must observe the kernel stop, verify the target's
//! cdhash and designated requirement, and only then send SIGCONT. A refusal
//! kills the still-suspended child. The v1 mode used by DisclaimedCommand is
//! unchanged and must not be used for supervised admission.
//!
//! A trampoline, rather than a `pre_exec` SETEXEC hook, is necessary because
//! after fork in a multithreaded process only async-signal-safe operations are
//! allowed. Constructing C strings, collecting the environment and setting up
//! `posix_spawn` allocate memory and can deadlock on a lock held at fork by
//! another thread. The fresh, single-threaded trampoline does that work safely;
//! the builder's only pre-exec work is allocation-free descriptor flag handling.
//!
//! ```no_run
//! use std::{process::Stdio, time::{Duration, Instant}};
//! use subc_os::privacy_identity::{self, DisclaimedCommand};
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let args: Vec<_> = std::env::args_os().skip(1).collect();
//!     privacy_identity::trampoline_main(&args); // First, before any runtime.
//!     let trampoline = std::env::current_exe()?;
//!     privacy_identity::probe(&trampoline, Instant::now() + Duration::from_secs(5))?;
//!     let mut builder = DisclaimedCommand::new(trampoline, "/bin/echo");
//!     builder.arg("hello").env("LANG", "C").stdout(Stdio::inherit());
//!     let (mut command, confirmation) = builder.into_command()?;
//!     let deadline = Instant::now() + Duration::from_secs(5);
//!     let mut child = command.spawn()?;
//!     drop(command); // Release the parent's ack writer before waiting for EOF.
//!     if let Err(error) = confirmation.confirm(deadline) {
//!         let _ = child.kill();
//!         let _ = child.wait();
//!         return Err(error.into());
//!     }
//!     child.wait()?;
//!     Ok(())
//! }
//! ```

mod command;

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod suspended_signature {
    use crate::code_signature::{self, CodeDirectoryHash, FileSignature};
    use std::{ffi::c_void, fs::File, io, os::unix::ffi::OsStrExt, path::Path, ptr};

    type Ref = *const c_void;

    // Load only in the verifier, not in the trampoline. Linking these frameworks
    // eagerly runs CoreFoundation initialization before main and adds an ambient
    // __CF_USER_TEXT_ENCODING variable to otherwise cleared child environments.
    macro_rules! framework_api {
        ($(fn $name:ident($($arg:ty),*) -> $result:ty;)+) => {
            #[allow(non_snake_case)]
            struct Api {
                $($name: unsafe extern "C" fn($($arg),*) -> $result,)+
                guest_pid_key: usize,
            }
            impl Api {
                #[allow(non_snake_case)]
                fn load() -> Option<Self> {
                    // SAFETY: fixed system framework paths and matching ABI
                    // declarations. Handles stay loaded for this process's life.
                    unsafe {
                        let cf = libc::dlopen(c"/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation".as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
                        let sec = libc::dlopen(c"/System/Library/Frameworks/Security.framework/Security".as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
                        if cf.is_null() || sec.is_null() { return None; }
                        let symbol = |name: *const libc::c_char| {
                            let value = libc::dlsym(cf, name);
                            if value.is_null() { libc::dlsym(sec, name) } else { value }
                        };
                        $(let $name = symbol(concat!(stringify!($name), "\0").as_ptr().cast());
                        if $name.is_null() { return None; })+
                        let key = symbol(c"kSecGuestAttributePid".as_ptr());
                        if key.is_null() { return None; }
                        Some(Self {
                            $($name: std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($arg),*) -> $result>($name),)+
                            guest_pid_key: *key.cast::<Ref>() as usize,
                        })
                    }
                }
            }
        };
    }

    framework_api! {
        fn CFRelease(Ref) -> ();
        fn CFURLCreateFromFileSystemRepresentation(Ref, *const u8, isize, u8) -> Ref;
        fn CFNumberCreate(Ref, isize, *const c_void) -> Ref;
        fn CFDictionaryCreate(Ref, *const Ref, *const Ref, isize, Ref, Ref) -> Ref;
        fn CFDataGetLength(Ref) -> isize;
        fn CFDataGetBytePtr(Ref) -> *const u8;
        fn CFDataCreate(Ref, *const u8, isize) -> Ref;
        fn SecStaticCodeCreateWithPath(Ref, u32, *mut Ref) -> i32;
        fn SecCodeCopyDesignatedRequirement(Ref, u32, *mut Ref) -> i32;
        fn SecRequirementCopyData(Ref, u32, *mut Ref) -> i32;
        fn SecRequirementCreateWithData(Ref, u32, *mut Ref) -> i32;
        fn SecCodeCopyGuestWithAttributes(Ref, Ref, u32, *mut Ref) -> i32;
        fn SecCodeCheckValidity(Ref, u32, Ref) -> i32;
    }

    fn api() -> io::Result<&'static Api> {
        static API: std::sync::OnceLock<Option<Api>> = std::sync::OnceLock::new();
        API.get_or_init(Api::load)
            .as_ref()
            .ok_or_else(|| io::Error::other("code verification frameworks unavailable"))
    }

    struct Owned(Ref, &'static Api);
    impl Owned {
        fn new(api: &'static Api, value: Ref) -> io::Result<Self> {
            if value.is_null() {
                Err(io::Error::other("code signature object unavailable"))
            } else {
                Ok(Self(value, api))
            }
        }
    }
    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: each value is a non-null owned Create/Copy reference.
            unsafe { (self.1.CFRelease)(self.0) }
        }
    }

    fn copied(api: &'static Api, call: impl FnOnce(*mut Ref) -> i32) -> io::Result<Owned> {
        let mut value = ptr::null();
        let status = call(&mut value);
        let value = Owned::new(api, value);
        if status != 0 {
            return Err(io::Error::other(format!(
                "code signature verification failed ({status})"
            )));
        }
        value
    }

    /// Launch-time signature of the retained executable. Only a stopped child
    /// matching both this cdhash set and designated requirement may be resumed.
    pub struct SuspendedSignature {
        hashes: Vec<CodeDirectoryHash>,
        requirement: Vec<u8>,
    }

    impl SuspendedSignature {
        pub fn capture(file: &File, path: &Path) -> io::Result<Self> {
            let hashes = match code_signature::file_signature(file, |_| {}) {
                Ok(FileSignature::Signed(hashes)) if !hashes.is_empty() => hashes,
                _ => return Err(io::Error::other("executable has no validated code directory (scripts are not executable images)")),
            };
            let bytes = path.as_os_str().as_bytes();
            let api = api()?;
            // SAFETY: the URL copies the live path bytes. All Security calls
            // receive owned CF references and writable out-pointers.
            let requirement = unsafe {
                let url = Owned::new(
                    api,
                    (api.CFURLCreateFromFileSystemRepresentation)(
                        ptr::null(),
                        bytes.as_ptr(),
                        bytes.len() as isize,
                        0,
                    ),
                )?;
                let code = copied(api, |out| (api.SecStaticCodeCreateWithPath)(url.0, 0, out))?;
                let requirement = copied(api, |out| {
                    (api.SecCodeCopyDesignatedRequirement)(code.0, 0, out)
                })?;
                let data = copied(api, |out| {
                    (api.SecRequirementCopyData)(requirement.0, 0, out)
                })?;
                let len = (api.CFDataGetLength)(data.0);
                if !(1..=1024 * 1024).contains(&len) {
                    return Err(io::Error::other("invalid designated requirement size"));
                }
                std::slice::from_raw_parts((api.CFDataGetBytePtr)(data.0), len as usize).to_vec()
            };
            Ok(Self {
                hashes,
                requirement,
            })
        }

        /// Observe a child stop without consuming its stop or exit status.
        /// Empty exec-ack EOF can precede the stop event, so callers must wait
        /// within their original launch deadline, never treat EOF as admission.
        pub fn is_suspended(pid: u32) -> io::Result<bool> {
            let mut status = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
            // SAFETY: status is writable and waitid addresses only this child.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid,
                    status.as_mut_ptr(),
                    libc::WSTOPPED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful waitid initialized status (zero pid means no event).
            let status = unsafe { status.assume_init() };
            // START_SUSPENDED reports CLD_STOPPED with status 0, not SIGSTOP.
            if status.si_pid != pid as i32 || status.si_code != libc::CLD_STOPPED {
                return Ok(false);
            }
            // WNOWAIT retains the stop event even after an outside SIGCONT.
            // Check the current kernel state as well, not just that old event.
            let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
            let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
            // SAFETY: the output buffer has the exact PROC_PIDTBSDINFO size.
            let count = unsafe {
                libc::proc_pidinfo(
                    pid as i32,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    info.as_mut_ptr().cast(),
                    size,
                )
            };
            if count != size {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a full successful proc_pidinfo call initialized info.
            let info = unsafe { info.assume_init() };
            const SSTOP: u32 = 4;
            Ok(info.pbi_pid == pid && info.pbi_status == SSTOP)
        }

        pub fn verify(&self, pid: u32) -> io::Result<()> {
            let stopped = || -> io::Result<()> {
                if Self::is_suspended(pid)? {
                    Ok(())
                } else {
                    Err(io::Error::other("target is not suspended"))
                }
            };
            let hash = || -> io::Result<CodeDirectoryHash> {
                let hash = code_signature::process_cdhash(pid).map_err(io::Error::other)?;
                if self.hashes.contains(&hash) {
                    Ok(hash)
                } else {
                    Err(io::Error::other("suspended target cdhash mismatch"))
                }
            };
            stopped()?;
            let before = hash()?;
            let pid_value = i32::try_from(pid).map_err(io::Error::other)?;
            let api = api()?;
            // SAFETY: the number and dictionary borrow live values until guest
            // resolution returns. The requirement data owns its copied bytes.
            unsafe {
                let number = Owned::new(
                    api,
                    (api.CFNumberCreate)(ptr::null(), 3, (&pid_value as *const i32).cast()),
                )?;
                let key = api.guest_pid_key as Ref;
                let attributes = Owned::new(
                    api,
                    (api.CFDictionaryCreate)(
                        ptr::null(),
                        &key,
                        &number.0,
                        1,
                        ptr::null(),
                        ptr::null(),
                    ),
                )?;
                let guest = copied(api, |out| {
                    (api.SecCodeCopyGuestWithAttributes)(ptr::null(), attributes.0, 0, out)
                })?;
                let data = Owned::new(
                    api,
                    (api.CFDataCreate)(
                        ptr::null(),
                        self.requirement.as_ptr(),
                        self.requirement.len() as isize,
                    ),
                )?;
                let requirement = copied(api, |out| {
                    (api.SecRequirementCreateWithData)(data.0, 0, out)
                })?;
                let status = (api.SecCodeCheckValidity)(guest.0, 0, requirement.0);
                if status != 0 {
                    return Err(io::Error::other(format!(
                        "suspended target designated requirement mismatch ({status})"
                    )));
                }
            }
            if before != hash()? {
                return Err(io::Error::other(
                    "suspended target changed during verification",
                ));
            }
            stopped()
        }

        /// Revalidate immediately before SIGCONT, including after an async
        /// admission check. Errors never send a resume signal.
        pub fn resume(&self, pid: u32) -> io::Result<()> {
            self.verify(pid)?;
            // SAFETY: verify established that pid is our stopped, unreaped child.
            if unsafe { libc::kill(pid as i32, libc::SIGCONT) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn suspended_signature_checks_designated_requirement_even_with_matching_cdhash() {
            let sleep = Path::new("/bin/sleep");
            let shell = Path::new("/bin/sh");
            let mut signature =
                SuspendedSignature::capture(&File::open(sleep).unwrap(), sleep).unwrap();
            let other = SuspendedSignature::capture(&File::open(shell).unwrap(), shell).unwrap();
            signature.requirement = other.requirement;
            // Exercise the same kernel stop used by SETEXEC, without needing an
            // embedding executable for this Security-framework unit test.
            struct Child(i32);
            impl Drop for Child {
                fn drop(&mut self) {
                    // SAFETY: this guard owns an unreaped test child.
                    unsafe {
                        libc::kill(self.0, libc::SIGKILL);
                        libc::waitpid(self.0, ptr::null_mut(), 0);
                    }
                }
            }
            // SAFETY: all argv pointers and the initialized attributes live
            // through posix_spawn. A guard reaps the child even on assertion panic.
            let child = unsafe {
                let mut attr = std::mem::MaybeUninit::<libc::posix_spawnattr_t>::uninit();
                assert_eq!(libc::posix_spawnattr_init(attr.as_mut_ptr()), 0);
                let mut attr = attr.assume_init();
                assert_eq!(
                    libc::posix_spawnattr_setflags(
                        &mut attr,
                        libc::POSIX_SPAWN_START_SUSPENDED as i16
                    ),
                    0
                );
                let argv = [
                    c"/bin/sleep".as_ptr().cast_mut(),
                    c"30".as_ptr().cast_mut(),
                    ptr::null_mut(),
                ];
                let envp = [ptr::null_mut()];
                let mut pid = 0;
                let result = libc::posix_spawn(
                    &mut pid,
                    c"/bin/sleep".as_ptr(),
                    ptr::null(),
                    &attr,
                    argv.as_ptr(),
                    envp.as_ptr(),
                );
                libc::posix_spawnattr_destroy(&mut attr);
                assert_eq!(result, 0);
                Child(pid)
            };
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !SuspendedSignature::is_suspended(child.0 as u32).unwrap() {
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let error = signature.resume(child.0 as u32).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("designated requirement mismatch"),
                "{error}"
            );
            assert!(SuspendedSignature::is_suspended(child.0 as u32).unwrap());
        }
    }
}

pub use command::{probe, ConfirmationError, DisclaimedCommand, ExecConfirmation, ExecError};

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
/// Capability required for supervised, verified-before-resume launches.
pub const SUSPENDED_TRAMPOLINE_PROBE: &str = "subc-privacy-trampoline/suspended-v2";
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
    let suspended = args
        .first()
        .is_some_and(|arg| arg == "__disclaim-exec-suspended");
    if !suspended && !args.first().is_some_and(|arg| arg == "__disclaim-exec") {
        return;
    }
    #[cfg(target_os = "macos")]
    {
        if args.len() == 2 && (args[1] == "--probe" || args[1] == "--probe-suspended") {
            #[cfg(feature = "test-support")]
            let missing =
                test_hooks && std::env::var_os("SUBC_TEST_PRIVACY_MISSING_SYMBOL").is_some();
            #[cfg(not(feature = "test-support"))]
            let missing = false;
            match macos::resolve_disclaim(missing) {
                Ok(_) => {
                    println!(
                        "{}",
                        if args[1] == "--probe-suspended" {
                            SUSPENDED_TRAMPOLINE_PROBE
                        } else {
                            TRAMPOLINE_PROBE
                        }
                    );
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
            let error = macos::exec_for_test(&args[1..], suspended)
                .expect_err("SETEXEC cannot return on success");
            eprintln!("ck-subc: own privacy identity refused: {error}");
            std::process::exit(error.exit_code());
        }
        let _ = test_hooks;
        let error = macos::exec_with_refusal(&args[1..], suspended)
            .expect_err("SETEXEC cannot return on success");
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
pub use macos::{disclaim_exec, ExecAcknowledgement};

#[cfg(target_os = "macos")]
pub use suspended_signature::SuspendedSignature;

#[cfg(all(target_os = "macos", feature = "test-support"))]
pub use macos::{disclaim_exec_for_test, privacy_observation_for_test};

#[cfg(target_os = "macos")]
mod macos {
    use super::ExecError;
    use std::{
        ffi::{CString, OsString},
        io,
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::ffi::OsStrExt,
        },
    };

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
            // SAFETY: duplicates the owned descriptor above the reserved slots
            // for stdio, nonce, registration and acknowledgement. The parent copy stays CLOEXEC; only our
            // child clears that flag.
            #[allow(unsafe_code)]
            let fd = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 6) };
            if fd == -1 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful fcntl returned a fresh descriptor owned here.
            #[allow(unsafe_code)]
            let writer = unsafe { OwnedFd::from_raw_fd(fd) };
            Ok((reader, Self { writer }))
        }

        pub fn fd(&self) -> i32 {
            5
        }

        /// Transfer the parent source to the supervisor's combined placement.
        pub fn into_descriptor(self) -> OwnedFd {
            self.writer
        }

        pub(super) fn into_writer(self) -> OwnedFd {
            self.writer
        }

        /// Compatibility handoff for callers without other inherited slots.
        /// Supervisors use the combined `ChildDescriptorSlots` placement instead.
        /// Retain `self` until spawn
        /// returns, then drop it and the Command so the parent holds no writer.
        pub fn install(&self, command: &mut std::process::Command) {
            use std::os::unix::process::CommandExt;
            let fd = self.writer.as_raw_fd();
            // SAFETY: pre_exec makes only fcntl calls, allocates nothing, and
            // touches no Rust locks. The captured descriptor is kept alive by
            // the caller until spawn completes and is never fd 3 or stdio.
            #[allow(unsafe_code)]
            unsafe {
                command.pre_exec(move || crate::registration_channel::place_descriptor(fd, 5));
            }
        }

        /// Keep the descriptor alive in the command, even if confirmation is
        /// dropped first. Dropping the command after spawn releases the parent's
        /// writer so the reader can observe EOF.
        pub(super) fn install_owned(self, command: &mut std::process::Command) {
            let mut slots = crate::registration_channel::ChildDescriptorSlots::default();
            // pipe() already placed the source above all reserved slots.
            slots.sources[2] = Some(self.writer);
            slots.install(command);
        }
    }

    #[cfg(test)]
    #[test]
    fn acknowledgement_source_avoids_channel_slot_and_inherits_slot_five() {
        const ISOLATED: &str = "SUBC_ACK_SLOT_TEST";
        if std::env::var_os(ISOLATED).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "acknowledgement_source_avoids_channel_slot_and_inherits_slot_five",
                    "--nocapture",
                ])
                .env(ISOLATED, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let (_reader, ack) = ExecAcknowledgement::pipe().unwrap();
        assert!(
            ack.writer.as_raw_fd() >= 6,
            "ack source collided with a child slot"
        );
        assert_eq!(ack.fd(), 5);
        // SAFETY: inspect the parent-owned source before spawn.
        #[allow(unsafe_code)]
        let flags = unsafe { libc::fcntl(ack.writer.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        let mut command = std::process::Command::new("/usr/bin/python3");
        command.args(["-c", "import fcntl,stat,os; assert fcntl.fcntl(5,fcntl.F_GETFD)==0; assert stat.S_ISFIFO(os.fstat(5).st_mode)"]);
        let mut slots = crate::registration_channel::ChildDescriptorSlots::default();
        slots.acknowledgement(ack.into_descriptor()).unwrap();
        slots.install(&mut command);
        let output = crate::child_spawn::output(&mut command).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Execute `<ack fd> <program> <args...>` as a new responsible process.
    /// Success never returns. This must run in a freshly exec'd, single-threaded
    /// trampoline, NOT in pre_exec: CString and environment construction allocate.
    pub fn disclaim_exec(args: &[OsString]) -> Result<(), ExecError> {
        exec_with_refusal(args, false)
    }

    pub(super) fn exec_with_refusal(args: &[OsString], suspended: bool) -> Result<(), ExecError> {
        let result = exec(args, false, suspended);
        if let Err(error) = &result {
            write_refusal_record(args, error);
        }
        result
    }

    /// Only the dedicated fixture binary calls this. No production config or
    /// environment variable can select another lookup symbol in `ck-subc`.
    #[cfg(feature = "test-support")]
    pub fn disclaim_exec_for_test(args: &[OsString]) -> Result<(), ExecError> {
        exec_for_test(args, false)
    }

    #[cfg(feature = "test-support")]
    pub(super) fn exec_for_test(args: &[OsString], suspended: bool) -> Result<(), ExecError> {
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
            suspended,
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
    fn exec(args: &[OsString], missing_symbol: bool, suspended: bool) -> Result<(), ExecError> {
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
                (libc::POSIX_SPAWN_SETEXEC
                    | if suspended {
                        libc::POSIX_SPAWN_START_SUSPENDED
                    } else {
                        0
                    }) as libc::c_short,
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
