use std::{
    process::Child,
    time::{Duration, Instant},
};

/// Retain the child until it is reaped, including during startup failures.
pub(crate) struct Process(pub Child);

impl Drop for Process {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(Some(_))) {
            return;
        }
        if let Ok(Some(process)) = subc_os::Process::open(self.0.id()) {
            let _ = process.signal(subc_os::Signal::Terminate);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if matches!(self.0.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
