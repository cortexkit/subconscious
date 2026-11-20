use rustix::event::{poll, PollFd, PollFlags};
use std::{
    fs::File,
    io::{self, Read},
    os::unix::net::UnixStream,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
};

/// Closing the supervisor's socket wakes both readers without waiting for the
/// last process with an inherited pipe writer to close it.
pub(crate) struct StopReaders {
    _wake_on_drop: UnixStream,
    finished: Arc<AtomicU8>,
}

impl StopReaders {
    pub(super) fn pair() -> io::Result<(Self, ReaderWake)> {
        let (send, receive) = UnixStream::pair()?;
        let finished = Arc::new(AtomicU8::new(0));
        Ok((
            Self {
                _wake_on_drop: send,
                finished: Arc::clone(&finished),
            },
            ReaderWake {
                socket: Arc::new(receive),
                finished,
            },
        ))
    }

    pub(crate) fn had_open_readers(self) -> bool {
        self.finished.load(Ordering::Acquire) != 0b11
    }
}

#[derive(Clone)]
pub(super) struct ReaderWake {
    socket: Arc<UnixStream>,
    finished: Arc<AtomicU8>,
}

impl ReaderWake {
    pub(super) fn finish(&self, pipe: super::Pipe) {
        self.finished.fetch_or(1 << (pipe as u8), Ordering::Release);
    }

    pub(super) fn read(&self, file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
        let mut fds = [
            PollFd::new(&*file, PollFlags::IN),
            PollFd::new(&*self.socket, PollFlags::IN),
        ];
        poll(&mut fds, None)?;
        if !fds[1].revents().is_empty() {
            return Err(io::Error::other(
                "late child output lost: capture pipe did not close",
            ));
        }
        // Only this thread can consume pipe data; readiness means the read can
        // return data, EOF, or an error without waiting for a new write.
        file.read(buf)
    }
}
