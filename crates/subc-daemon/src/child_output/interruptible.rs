use crate::supervise::live_upgrade::output::{Control, Park};
use std::{
    io,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
};

#[cfg(test)]
thread_local! {
    static FAIL_PAIR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn fail_next_pair_for_test() {
    FAIL_PAIR.set(true);
}

/// Closing the supervisor's socket wakes both readers without waiting for the
/// last process with an inherited pipe writer to close it.
pub(crate) struct StopReaders {
    control: Arc<Control>,
    finished: Arc<AtomicU8>,
    stop_on_drop: bool,
}

impl StopReaders {
    pub(super) fn pair(stop_on_drop: bool) -> io::Result<(Self, ReaderWake)> {
        #[cfg(test)]
        if FAIL_PAIR.replace(false) {
            return Err(rustix::io::Errno::MFILE.into());
        }
        let control = Control::new()?;
        let finished = Arc::new(AtomicU8::new(0));
        Ok((
            Self {
                control: control.clone(),
                finished: Arc::clone(&finished),
                stop_on_drop,
            },
            ReaderWake { control, finished },
        ))
    }

    pub(crate) fn had_open_readers(self) -> bool {
        let open = self.finished.load(Ordering::Acquire) != 0b11;
        self.control.stop();
        open
    }

    pub(crate) fn park(&self) -> io::Result<Park> {
        self.control.park()
    }
}

impl Drop for StopReaders {
    fn drop(&mut self) {
        if self.stop_on_drop {
            self.control.stop();
        }
    }
}

#[derive(Clone)]
pub(super) struct ReaderWake {
    pub(super) control: Arc<Control>,
    finished: Arc<AtomicU8>,
}

impl ReaderWake {
    pub(super) fn finish(&self, pipe: super::Pipe) {
        self.finished.fetch_or(1 << (pipe as u8), Ordering::Release);
    }
}
