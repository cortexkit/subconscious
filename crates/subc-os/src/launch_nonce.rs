//! The launch nonce: the secret the daemon gives each module it spawns, which
//! the module presents to be admitted as itself.
//!
//! On macOS and Linux the daemon hands it over through a pipe rather than
//! the environment, because any process of the same user can read another
//! process's initial environment (`ps eww`, `sysctl KERN_PROCARGS2`). The
//! daemon side is [`LaunchNonceHandoff`]: a pipe that already holds the nonce,
//! whose read end becomes descriptor [`LAUNCH_NONCE_FD`] in the child. The
//! module side is [`launch_nonce`]: it reads that descriptor once, closes it,
//! and caches the value for the life of the process.
//!
//! Windows uses a one-time named pipe authenticated to the direct child's PID.
//! The daemon still sets the Windows environment copy for readers that have
//! not adopted the pipe. `SUBC_LAUNCH_NONCE_PIPE_FALLBACK=env` explicitly permits
//! environment startup when delivery fails; unmarked readers remain fail-closed.

use std::{
    ffi::OsString,
    fmt,
    sync::{atomic::AtomicUsize, OnceLock},
};

/// The descriptor number the pipe's read end has in the child.
pub const LAUNCH_NONCE_FD: i32 = 3;

/// Names the descriptor holding the nonce, as `<fd>:<inode>`. The inode names
/// the pipe itself, so the reader can tell it from an unrelated descriptor
/// that happens to have the same number. A process a module spawns inherits
/// this variable but not the pipe, and without the inode it would read and
/// close whatever that process has at the number.
pub const LAUNCH_NONCE_FD_ENV: &str = "SUBC_LAUNCH_NONCE_FD";

/// Names the one-time Windows pipe. Its name is public, not a credential.
pub const LAUNCH_NONCE_PIPE_ENV: &str = "SUBC_LAUNCH_NONCE_PIPE";

/// Explicit Windows rollout permission to use the environment copy if pipe
/// delivery fails. The daemon sets this to `env` while environment-only modules
/// migrate to pipe reads; it must be removed with the environment copy.
pub const LAUNCH_NONCE_PIPE_FALLBACK_ENV: &str = "SUBC_LAUNCH_NONCE_PIPE_FALLBACK";

/// The environment copy of the nonce, kept only while modules move to the
/// descriptor. Same name as `subc_protocol::SUBC_LAUNCH_NONCE_ENV`; this crate
/// does not depend on subc-protocol, so it states the name itself.
pub const LAUNCH_NONCE_ENV: &str = "SUBC_LAUNCH_NONCE";

/// Where a process got its launch nonce from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LaunchNonceSource {
    /// The inherited descriptor named by [`LAUNCH_NONCE_FD_ENV`].
    Fd,
    /// The PID-authenticated Windows pipe named by [`LAUNCH_NONCE_PIPE_ENV`].
    Pipe,
    /// The environment variable [`LAUNCH_NONCE_ENV`].
    Env,
}

impl LaunchNonceSource {
    /// The name modules report in their provenance.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fd => "fd",
            Self::Pipe => "pipe",
            Self::Env => "env",
        }
    }
}

/// The nonce this process was launched with, and where it came from.
/// `Debug` never prints the value.
#[derive(Clone, PartialEq, Eq)]
pub struct LaunchNonce {
    value: String,
    source: LaunchNonceSource,
}

impl LaunchNonce {
    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn source(&self) -> LaunchNonceSource {
        self.source
    }
}

impl fmt::Debug for LaunchNonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaunchNonce")
            .field(
                "value",
                &format_args!("<{} bytes redacted>", self.value.len()),
            )
            .field("source", &self.source)
            .finish()
    }
}

/// Why a named launch-nonce handoff gave no nonce.
///
/// These errors never fall back unless a Windows daemon explicitly permits
/// an environment copy via [`LAUNCH_NONCE_PIPE_FALLBACK_ENV`].
/// A named descriptor or pipe
/// that cannot be read means the handoff went wrong, or that this process
/// inherited the variable from a module without inheriting the pipe; reading
/// the environment instead would hide a failed handoff or admit a descendant
/// using its parent's environment credential.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LaunchNonceError {
    /// The variable is not `<fd>:<inode>`.
    Malformed { value: String },
    /// Nothing is open at that number: the variable was inherited without
    /// the descriptor, which is what a process spawned by a module sees.
    NotOpen { fd: i32, errno: i32 },
    /// The descriptor is open but is not a pipe. It was left alone.
    NotAPipe { fd: i32 },
    /// The descriptor is a pipe, but not the one named. It was left alone.
    WrongPipe {
        fd: i32,
        expected_inode: u64,
        found_inode: u64,
    },
    /// The named pipe holds no bytes. It was left open and unread.
    Empty { fd: i32 },
    /// Reading the named pipe failed.
    Unreadable { fd: i32, errno: Option<i32> },
    /// The named pipe held bytes that are not UTF-8.
    NotUtf8 { fd: i32 },
    /// The named Windows pipe could not be opened within the busy retry window.
    PipeNotOpen { errno: i32 },
    /// The named Windows pipe held no bytes.
    PipeEmpty,
    /// Reading the Windows pipe failed or exceeded its deadline.
    PipeUnreadable { errno: Option<i32> },
    /// The Windows pipe held bytes that are not UTF-8.
    PipeNotUtf8,
}

impl fmt::Display for LaunchNonceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PipeNotOpen { errno } => write!(f, "{LAUNCH_NONCE_PIPE_ENV} names a pipe that could not be opened (errno {errno})"),
            Self::PipeEmpty => write!(f, "the launch nonce named pipe is empty"),
            Self::PipeUnreadable { errno } => write!(f, "could not read the launch nonce from the named pipe (errno {errno:?})"),
            Self::PipeNotUtf8 => write!(f, "the launch nonce named pipe held bytes that are not UTF-8"),
            Self::Malformed { value } => write!(
                f,
                "{LAUNCH_NONCE_FD_ENV}={value:?} is not <fd>:<inode>"
            ),
            Self::NotOpen { fd, errno } => write!(
                f,
                "{LAUNCH_NONCE_FD_ENV} names descriptor {fd}, which is not open (errno {errno}); \
                 a process spawned by a module inherits the variable but not the descriptor"
            ),
            Self::NotAPipe { fd } => write!(
                f,
                "{LAUNCH_NONCE_FD_ENV} names descriptor {fd}, which is not a pipe; left it untouched"
            ),
            Self::WrongPipe {
                fd,
                expected_inode,
                found_inode,
            } => write!(
                f,
                "{LAUNCH_NONCE_FD_ENV} names descriptor {fd} with inode {expected_inode}, but it has \
                 inode {found_inode}; left it untouched"
            ),
            Self::Empty { fd } => write!(
                f,
                "the launch nonce pipe at descriptor {fd} is empty; left it untouched"
            ),
            Self::Unreadable { fd, errno } => write!(
                f,
                "could not read the launch nonce from descriptor {fd} (errno {errno:?})"
            ),
            Self::NotUtf8 { fd } => write!(
                f,
                "the launch nonce pipe at descriptor {fd} held bytes that are not UTF-8"
            ),
        }
    }
}

impl std::error::Error for LaunchNonceError {}

type Cached = Result<Option<LaunchNonce>, LaunchNonceError>;

/// A launch nonce read at most once. The process has one, behind
/// [`launch_nonce`]; tests make their own with a stand-in for the
/// environment.
pub(crate) struct LaunchNonceCell {
    value: OnceLock<Cached>,
    descriptor_reads: AtomicUsize,
}

impl LaunchNonceCell {
    pub(crate) const fn new() -> Self {
        Self {
            value: OnceLock::new(),
            descriptor_reads: AtomicUsize::new(0),
        }
    }

    /// The cached result, reading it first if no caller has yet. Concurrent
    /// first callers wait for the one that reads, so nobody reads twice.
    pub(crate) fn get(&self, lookup: impl FnMut(&str) -> Option<OsString>) -> Cached {
        self.value
            .get_or_init(|| read_launch_nonce(lookup, &self.descriptor_reads))
            .clone()
    }

    /// How many times this cell has taken a descriptor. At most one.
    #[cfg(all(test, unix))]
    pub(crate) fn descriptor_reads(&self) -> usize {
        self.descriptor_reads
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

static PROCESS_NONCE: LaunchNonceCell = LaunchNonceCell::new();

/// This process's launch nonce and where it came from.
///
/// The first call decides, and every later call returns the same answer
/// without touching the descriptor or the environment again. So every reader
/// in a process must come through here: after the first read closes the
/// descriptor, its number is the next one the process hands out, and a second
/// independent reader would read and close some unrelated socket or file.
///
/// - When [`LAUNCH_NONCE_FD_ENV`] is set (macOS and Linux), the descriptor it
///   names is taken only if it is a pipe with the named inode and holds
///   bytes; it is then read to end of file and closed. Anything else is a
///   [`LaunchNonceError`] that leaves the descriptor as it was and never
///   falls back to the environment.
/// - On Windows, when [`LAUNCH_NONCE_PIPE_ENV`] is set, the named pipe is opened
///   read-only at identification impersonation level and read to EOF with a
///   bounded deadline. Without explicit fallback permission, errors are cached
///   and never fall back. When [`LAUNCH_NONCE_PIPE_FALLBACK_ENV`] is `env` and
///   a nonempty environment copy exists, failed or incomplete delivery instead
///   caches that copy and its `env` source. A matching complete read stays `pipe`.
/// - Otherwise the value of [`LAUNCH_NONCE_ENV`] is used.
/// - `Ok(None)` means no handoff is named (or the environment copy is empty): the
///   process was not started by the daemon.
///
/// It never changes the environment. Removing either variable would break
/// any other reader in the process still on the environment copy, and
/// changing the environment of a multi-threaded process can race readers in
/// libraries whose environment access cannot be synchronized by this accessor.
///
/// Call it before the process spawns anything. Until the first read the
/// descriptor is inheritable (it has to be, to survive the daemon's exec),
/// so a child spawned earlier could inherit and consume the module's secret.
pub fn launch_nonce() -> Result<Option<LaunchNonce>, LaunchNonceError> {
    PROCESS_NONCE.get(|key| std::env::var_os(key))
}

fn read_launch_nonce(
    mut lookup: impl FnMut(&str) -> Option<OsString>,
    descriptor_reads: &AtomicUsize,
) -> Cached {
    #[cfg(windows)]
    if let Some(value) = lookup(LAUNCH_NONCE_PIPE_ENV) {
        let fallback = if lookup(LAUNCH_NONCE_PIPE_FALLBACK_ENV).as_deref()
            == Some(std::ffi::OsStr::new("env"))
        {
            environment_nonce(lookup(LAUNCH_NONCE_ENV))
        } else {
            None
        };
        return pipe_or_permitted_env(windows::read_pipe(&value), fallback);
    }
    #[cfg(unix)]
    if let Some(value) = lookup(LAUNCH_NONCE_FD_ENV) {
        return unix::read_descriptor(&value, descriptor_reads);
    }
    #[cfg(not(unix))]
    let _ = descriptor_reads;
    Ok(environment_nonce(lookup(LAUNCH_NONCE_ENV)))
}

fn environment_nonce(value: Option<OsString>) -> Option<LaunchNonce> {
    value
        .and_then(|value| value.into_string().ok())
        .filter(|value| !value.is_empty())
        .map(|value| LaunchNonce {
            value,
            source: LaunchNonceSource::Env,
        })
}

#[cfg(any(windows, test))]
fn pipe_or_permitted_env(pipe: Cached, fallback: Option<LaunchNonce>) -> Cached {
    let Some(fallback) = fallback else {
        return pipe;
    };
    match &pipe {
        Ok(Some(nonce)) if nonce.value() == fallback.value() => pipe,
        // A truncated but valid UTF-8 read is not a successful handoff. When
        // fallback is permitted, both transports carry the same nonce, so compare them too.
        _ => Ok(Some(fallback)),
    }
}

#[cfg(unix)]
pub use unix::LaunchNonceHandoff;

#[cfg(windows)]
pub use windows::{LaunchNoncePipeDelivery, LaunchNoncePipeHandoff};

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows;

#[cfg(unix)]
mod unix {
    use std::{
        ffi::OsStr,
        fs::File,
        io::{self, Read, Write},
        os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::{Cached, LaunchNonce, LaunchNonceError, LaunchNonceSource, LAUNCH_NONCE_FD};

    /// The daemon half of the handoff, prepared before the spawn: a pipe that
    /// already holds the nonce, its write end closed, and its inode.
    ///
    /// A module uses the same type to hand its own nonce to a helper process
    /// that must connect as the module: never in argv, the environment or a
    /// file, all of which another same-user process can read.
    ///
    /// ```no_run
    /// # fn helper(nonce: &str) -> std::io::Result<()> {
    /// use subc_os::launch_nonce::{LaunchNonceHandoff, LAUNCH_NONCE_FD_ENV};
    ///
    /// let mut command = std::process::Command::new("helper");
    /// let handoff = LaunchNonceHandoff::new(nonce)?;
    /// command.env(LAUNCH_NONCE_FD_ENV, handoff.fd_env_value());
    /// // After every other pre-exec step the command has.
    /// handoff.install_last(&mut command);
    /// command.spawn()?;
    /// # Ok(()) }
    /// ```
    #[derive(Debug)]
    pub struct LaunchNonceHandoff {
        read_end: OwnedFd,
        inode: u64,
        target: RawFd,
    }

    impl LaunchNonceHandoff {
        /// Make the pipe, write `nonce` into it and close the write end, so a
        /// reader gets exactly `nonce` and then end of file.
        ///
        /// Both ends are created close-on-exec (`std::io::pipe` does that),
        /// so the read end reaches no child until [`Self::install_last`] puts
        /// it into one. The nonce must fit in the pipe buffer, or the write
        /// blocks with no reader: POSIX guarantees 512 bytes and macOS and
        /// Linux give 16 KiB or more, and a daemon nonce is 64 characters.
        pub fn new(nonce: &str) -> io::Result<Self> {
            let (reader, mut writer) = io::pipe()?;
            writer.write_all(nonce.as_bytes())?;
            drop(writer);
            let mut read_end = OwnedFd::from(reader);
            // std configures the child's stdio before pre-exec callbacks. If a
            // standard descriptor was closed in the parent, pipe() can use its
            // number, which stdio setup would overwrite before the handoff runs.
            // Move it out of that range now, keeping the parent copy close-on-exec.
            if read_end.as_raw_fd() < LAUNCH_NONCE_FD {
                // SAFETY: duplicates an owned descriptor; no memory is passed.
                #[allow(unsafe_code)]
                let copy = unsafe {
                    libc::fcntl(read_end.as_raw_fd(), libc::F_DUPFD_CLOEXEC, LAUNCH_NONCE_FD)
                };
                if copy == -1 {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: the successful fcntl returned a new descriptor owned here.
                #[allow(unsafe_code)]
                {
                    read_end = unsafe { OwnedFd::from_raw_fd(copy) };
                }
            }
            let inode = fstat(read_end.as_raw_fd())?.st_ino as u64;
            Ok(Self {
                read_end,
                inode,
                target: LAUNCH_NONCE_FD,
            })
        }

        /// The value to give the child as
        /// [`LAUNCH_NONCE_FD_ENV`](super::LAUNCH_NONCE_FD_ENV):
        /// `3:<inode of this pipe>`.
        pub fn fd_env_value(&self) -> String {
            format!("{}:{}", self.target, self.inode)
        }

        /// Arrange for the read end to be descriptor 3 in the process
        /// `command` spawns, and in no other process. For a tokio `Command`,
        /// pass `command.as_std_mut()`.
        ///
        /// It must be the LAST pre-exec step registered. The standard library
        /// runs pre-exec steps in registration order, after its own stdio
        /// setup, and this one closes whatever the child had at descriptor 3.
        /// A step that runs later and writes through a descriptor it captured
        /// (the Linux cgroup placement writes to `cgroup.procs`) would find
        /// the pipe there instead if that descriptor had number 3.
        ///
        /// Registering any pre-exec step makes the standard library fork and
        /// exec instead of using `posix_spawn`, on macOS as on Linux.
        pub fn install_last(self, command: &mut std::process::Command) {
            use std::os::unix::process::CommandExt;
            // SAFETY: the closure runs between fork and exec in a copy of a
            // possibly multi-threaded process, where only async-signal-safe
            // calls are sound. `install_in_child` makes only dup2 and fcntl
            // calls on a descriptor opened before the fork, and allocates
            // nothing (see the_pre_exec_step_does_not_allocate).
            #[allow(unsafe_code)]
            unsafe {
                command.pre_exec(move || self.install_in_child());
            }
        }

        /// Put the read end at the target number without close-on-exec. Runs
        /// in the forked child: only dup2 and fcntl, and errors built from
        /// errno, which does not allocate.
        ///
        /// When the read end already has the target number, `dup2` would do
        /// nothing and leave close-on-exec set, so exec would close the
        /// descriptor; that case clears the flag instead.
        pub(crate) fn install_in_child(&self) -> io::Result<()> {
            let source = self.read_end.as_raw_fd();
            if source == self.target {
                // SAFETY: fcntl on a descriptor this struct owns; no memory is passed.
                #[allow(unsafe_code)]
                let flags = unsafe { libc::fcntl(source, libc::F_GETFD) };
                if flags == -1 {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: as above.
                #[allow(unsafe_code)]
                let set = unsafe { libc::fcntl(source, libc::F_SETFD, flags & !libc::FD_CLOEXEC) };
                if set == -1 {
                    return Err(io::Error::last_os_error());
                }
                return Ok(());
            }
            // SAFETY: dup2 takes two integers and touches no memory. It closes
            // whatever the child had at the target, which is why this step
            // must run after every other one (see `install_last`). The new
            // descriptor does not carry close-on-exec, so it survives exec.
            #[allow(unsafe_code)]
            if unsafe { libc::dup2(source, self.target) } == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// A handoff whose read end is placed at `target` instead of 3, so
        /// tests can exercise the step without claiming descriptor 3 in the
        /// test process itself.
        #[cfg(test)]
        pub(crate) fn with_target(mut self, target: RawFd) -> Self {
            self.target = target;
            self
        }

        #[cfg(test)]
        pub(crate) fn read_end_fd(&self) -> RawFd {
            self.read_end.as_raw_fd()
        }

        #[cfg(test)]
        pub(crate) fn inode(&self) -> u64 {
            self.inode
        }
    }

    pub(super) fn read_descriptor(value: &OsStr, descriptor_reads: &AtomicUsize) -> Cached {
        let text = value.to_string_lossy();
        let malformed = || LaunchNonceError::Malformed {
            value: text.to_string(),
        };
        let (fd_text, inode_text) = text.split_once(':').ok_or_else(malformed)?;
        let fd: RawFd = fd_text.parse().map_err(|_| malformed())?;
        let expected_inode: u64 = inode_text.parse().map_err(|_| malformed())?;
        if fd < 0 {
            return Err(malformed());
        }

        // Check what the descriptor is before taking ownership of it. Reading
        // and closing a descriptor that belongs to other code in this process
        // would break that code, and a process that inherited the variable
        // without the descriptor may well have something else at this number.
        let stat = fstat(fd).map_err(|error| LaunchNonceError::NotOpen {
            fd,
            errno: error.raw_os_error().unwrap_or(0),
        })?;
        if stat.st_mode & libc::S_IFMT != libc::S_IFIFO {
            return Err(LaunchNonceError::NotAPipe { fd });
        }
        let found_inode = stat.st_ino as u64;
        if found_inode != expected_inode {
            return Err(LaunchNonceError::WrongPipe {
                fd,
                expected_inode,
                found_inode,
            });
        }
        // Ask how many bytes are waiting rather than reading to find out, so
        // an empty pipe is refused without being consumed or closed.
        let mut waiting: libc::c_int = 0;
        // SAFETY: FIONREAD writes one int through the pointer, which points
        // at a live local of that type.
        #[allow(unsafe_code)]
        if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut waiting) } == -1 {
            return Err(LaunchNonceError::Unreadable {
                fd,
                errno: io::Error::last_os_error().raw_os_error(),
            });
        }
        if waiting <= 0 {
            return Err(LaunchNonceError::Empty { fd });
        }

        descriptor_reads.fetch_add(1, Ordering::SeqCst);
        // SAFETY: the descriptor is open and is the pipe the daemon named by
        // inode, so it was handed to this process for this read. Nothing else
        // in the process reads it: every reader goes through the one cached
        // accessor, which reaches this line at most once.
        #[allow(unsafe_code)]
        let mut file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        let mut bytes = Vec::with_capacity(64);
        let read = file.read_to_end(&mut bytes);
        drop(file);
        read.map_err(|error| LaunchNonceError::Unreadable {
            fd,
            errno: error.raw_os_error(),
        })?;
        let value = String::from_utf8(bytes).map_err(|_| LaunchNonceError::NotUtf8 { fd })?;
        Ok(Some(LaunchNonce {
            value,
            source: LaunchNonceSource::Fd,
        }))
    }

    pub(super) fn fstat(fd: RawFd) -> io::Result<libc::stat> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: fstat writes one `struct stat` into the buffer, which is
        // exactly that size, and writes nothing when it fails.
        #[allow(unsafe_code)]
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fstat succeeded, so it filled the buffer.
        #[allow(unsafe_code)]
        Ok(unsafe { stat.assume_init() })
    }
}

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
mod pipe_rollout_tests {
    use super::*;

    fn nonce(value: &str, source: LaunchNonceSource) -> LaunchNonce {
        LaunchNonce {
            value: value.to_owned(),
            source,
        }
    }

    #[test]
    fn phase_one_pipe_failure_uses_the_permitted_environment_source() {
        let result = pipe_or_permitted_env(
            Err(LaunchNonceError::PipeEmpty),
            Some(nonce("env-secret", LaunchNonceSource::Env)),
        );
        let value = result.unwrap().unwrap();
        assert_eq!(value.value(), "env-secret");
        assert_eq!(value.source(), LaunchNonceSource::Env);
    }

    #[test]
    fn phase_one_partial_pipe_nonce_uses_the_permitted_environment_source() {
        let result = pipe_or_permitted_env(
            Ok(Some(nonce("partial", LaunchNonceSource::Pipe))),
            Some(nonce("env-secret", LaunchNonceSource::Env)),
        );
        let value = result.unwrap().unwrap();
        assert_eq!(value.value(), "env-secret");
        assert_eq!(value.source(), LaunchNonceSource::Env);
    }

    #[test]
    fn phase_one_complete_pipe_nonce_keeps_the_pipe_source() {
        let result = pipe_or_permitted_env(
            Ok(Some(nonce("same-secret", LaunchNonceSource::Pipe))),
            Some(nonce("same-secret", LaunchNonceSource::Env)),
        );
        let value = result.unwrap().unwrap();
        assert_eq!(value.value(), "same-secret");
        assert_eq!(value.source(), LaunchNonceSource::Pipe);
    }

    #[test]
    fn strict_pipe_failure_without_permission_stays_an_error() {
        assert_eq!(
            pipe_or_permitted_env(Err(LaunchNonceError::PipeEmpty), None),
            Err(LaunchNonceError::PipeEmpty)
        );
    }
}
