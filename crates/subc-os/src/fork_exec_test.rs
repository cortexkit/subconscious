//! Test-only control of the interval in which a forked child still holds the
//! parent's close-on-exec descriptors. Nothing here runs in a shipped daemon.
//! Also observes an owned child's exit without consuming its wait status.

use std::{
    io,
    os::{fd::AsRawFd, unix::process::CommandExt},
};

/// Pause `command` before its first exec. The ready pipe carries eight bytes:
/// the child's pid and the observed descriptor's F_GETFD flags, both native-endian
/// i32 values. Writing one byte to the release pipe lets exec continue.
///
/// Keep the observed descriptor open until readiness is read, and always release
/// and reap the child, including on a failed assertion. The Command owns its two
/// child pipe ends until spawn returns; all four ends stay close-on-exec.
pub fn pause_before_exec(
    command: &mut std::process::Command,
    observed_fd: i32,
) -> io::Result<(std::io::PipeReader, std::io::PipeWriter)> {
    let (ready_reader, ready_writer) = io::pipe()?;
    let (release_reader, release_writer) = io::pipe()?;
    // SAFETY: this callback runs after fork in a possibly multithreaded process.
    // It uses only fcntl, getpid, read and write, fixed stack buffers and errno
    // errors: no allocation, Rust locks or other non-async-signal-safe work.
    // The captured pipe endpoints stay owned by the Command until spawn returns.
    #[allow(unsafe_code)]
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(observed_fd, libc::F_GETFD);
            if flags == -1 {
                return Err(io::Error::last_os_error());
            }
            let mut ready = [0u8; 8];
            ready[..4].copy_from_slice(&libc::getpid().to_ne_bytes());
            ready[4..].copy_from_slice(&flags.to_ne_bytes());
            let mut written = 0;
            while written < ready.len() {
                let count = libc::write(
                    ready_writer.as_raw_fd(),
                    ready[written..].as_ptr().cast(),
                    ready.len() - written,
                );
                if count > 0 {
                    written += count as usize;
                } else if count == -1
                    && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
                {
                    continue;
                } else {
                    return Err(io::Error::last_os_error());
                }
            }
            let mut release = [0u8; 1];
            loop {
                match libc::read(release_reader.as_raw_fd(), release.as_mut_ptr().cast(), 1) {
                    1 => return Ok(()),
                    -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
                    0 => return Err(io::Error::from_raw_os_error(libc::EPIPE)),
                    _ => return Err(io::Error::last_os_error()),
                }
            }
        });
    }
    Ok((ready_reader, release_writer))
}

/// Whether the flags a paused fork observed still include close-on-exec.
pub fn flags_are_close_on_exec(flags: i32) -> bool {
    flags & libc::FD_CLOEXEC != 0
}

/// Wait for an owned child's exit without reaping it. The caller must retain
/// the child handle and prevent any other task from waiting on that child until
/// this returns, then reap it normally. Intended for short-lived test modules:
/// this blocks until exit, not just until the module closes a pipe before exit.
#[cfg(target_os = "macos")]
pub fn wait_for_child_exit_without_reaping(pid: u32) -> io::Result<()> {
    // SAFETY: waitid writes only to the local initialized siginfo_t. WNOWAIT
    // leaves the exit status for the owning supervisor's subsequent try_wait.
    #[allow(unsafe_code)]
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        loop {
            if libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}
