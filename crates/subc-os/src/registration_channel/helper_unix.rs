#![allow(clippy::unnecessary_cast)] // ino_t differs across Unix targets.

use super::{check_claim, HELPER_CLAIM_FD_ENV};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{self, Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

/// A close-on-exec one-read pipe, reserved before a runtime spawns its child.
/// Only the stdio duplicate in the child is inheritable. Drop aborts delivery.
#[derive(Debug)]
pub struct PreparedHelperClaimHandoff {
    reader: OwnedFd,
    writer: Option<std::io::PipeWriter>,
    target: i32,
    inode: u64,
}

#[allow(unsafe_code)]
impl PreparedHelperClaimHandoff {
    pub fn new(child_fd: i32) -> io::Result<Self> {
        if child_fd < 3 {
            return Err(io::Error::other("helper requires a dedicated stdio slot"));
        }
        let (reader, writer) = io::pipe()?;
        let mut reader = OwnedFd::from(reader);
        if reader.as_raw_fd() < 3 {
            // SAFETY: duplicate a live descriptor, preserving close-on-exec.
            let fd = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
            if fd == -1 {
                return Err(io::Error::last_os_error());
            }
            reader = unsafe { OwnedFd::from_raw_fd(fd) };
        }
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: fstat fills this output on success.
        if unsafe { libc::fstat(reader.as_raw_fd(), stat.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let inode = unsafe { stat.assume_init() }.st_ino as u64;
        Ok(Self {
            reader,
            writer: Some(writer),
            target: child_fd,
            inode,
        })
    }
    pub fn environment(&self) -> Vec<(OsString, OsString)> {
        vec![(
            HELPER_CLAIM_FD_ENV.into(),
            format!("{}:{}", self.target, self.inode).into(),
        )]
    }
    pub fn child_fd(&self) -> Option<i32> {
        Some(self.reader.as_raw_fd())
    }
    pub fn complete(self, _child_pid: u32, claim: &str) -> io::Result<()> {
        let Self {
            reader, mut writer, ..
        } = self;
        drop(reader);
        write_claim(writer.as_mut(), claim)
    }
    pub fn abort(self) {}
    pub(super) fn write_claim(&mut self, claim: &str) -> io::Result<()> {
        write_claim(self.writer.as_mut(), claim)?;
        self.writer.take();
        Ok(())
    }
    pub(super) fn install_last(self, command: &mut std::process::Command) {
        use std::os::unix::process::CommandExt;
        command.envs(self.environment());
        // SAFETY: the post-fork closure only calls async-signal-safe fd operations.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(self.reader.as_raw_fd(), self.target) == -1
                    || libc::fcntl(self.target, libc::F_SETFD, 0) == -1
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

fn write_claim(writer: Option<&mut std::io::PipeWriter>, claim: &str) -> io::Result<()> {
    check_claim(claim)?;
    let claim = zeroize::Zeroizing::new(claim.to_owned());
    writer
        .ok_or_else(|| io::Error::other("helper handoff completed"))?
        .write_all(claim.as_bytes())
}

#[allow(unsafe_code)]
pub(super) fn read(value: &OsStr) -> io::Result<Option<String>> {
    let (fd, inode) = value
        .to_str()
        .and_then(|v| v.split_once(':'))
        .and_then(|(fd, ino)| Some((fd.parse::<i32>().ok()?, ino.parse::<u64>().ok()?)))
        .filter(|(fd, _)| *fd >= 3)
        .ok_or_else(|| io::Error::other("invalid helper metadata"))?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: validate before taking ownership of an inherited descriptor.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFIFO || stat.st_ino as u64 != inode {
        return Err(io::Error::other("invalid helper descriptor"));
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    loop {
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_millis()
            .min(5000) as i32;
        if ms == 0 || unsafe { libc::poll(&mut poll, 1, ms) } <= 0 {
            return Err(io::Error::other("helper handoff timed out"));
        }
        let mut chunk = [0; 513];
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..n]);
        if bytes.len() > 512 {
            return Err(io::Error::other("invalid helper claim length"));
        }
    }
    let value =
        std::str::from_utf8(&bytes).map_err(|_| io::Error::other("invalid helper claim"))?;
    check_claim(value)?;
    Ok(Some(value.to_owned()))
}
