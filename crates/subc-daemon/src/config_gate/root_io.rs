//! Dedicated workers for filesystem access to registry roots. The queue lock
//! protects bookkeeping only; no filesystem work runs while it is held.

use std::{
    collections::{BTreeMap, VecDeque},
    io,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use subc_control::config::ConfigRootIo;
use tokio::sync::oneshot;

const ROOT_IO_WORKERS: usize = 4;
const ROOT_IO_QUEUE_CAPACITY: usize = 16;
const ROOT_IO_TIMEOUT: Duration = Duration::from_secs(10);

/// A daemon's lane for operations on potentially unresponsive registry roots.
pub struct RootIoPool {
    shared: Arc<Shared>,
    timeout: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RootIoError {
    DeadlineElapsed,
    RootBusy,
    WorkersCharged,
    QueueFull,
    WorkerStopped,
}

impl From<RootIoError> for super::GateError {
    fn from(error: RootIoError) -> Self {
        let detail = match error {
            RootIoError::DeadlineElapsed => "registry-root I/O deadline elapsed",
            RootIoError::RootBusy => "registry-root I/O already outstanding for this root",
            RootIoError::WorkersCharged => "all registry-root I/O workers are charged",
            RootIoError::QueueFull => "registry-root I/O queue is full",
            RootIoError::WorkerStopped => "registry-root I/O worker stopped",
        };
        Self::new("config_unavailable", detail)
    }
}

pub(super) struct RootIoJob<T> {
    shared: Arc<Shared>,
    deadline: Instant,
    result: oneshot::Receiver<Result<T, RootIoError>>,
}

impl<T> RootIoJob<T> {
    pub(super) async fn wait(self) -> Result<T, RootIoError> {
        match tokio::time::timeout_at(self.deadline.into(), self.result).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) if Instant::now() < self.deadline => Err(RootIoError::WorkerStopped),
            _ => {
                let expired = self
                    .shared
                    .state
                    .lock()
                    .unwrap()
                    .take_expired(Instant::now());
                drop(expired);
                Err(RootIoError::DeadlineElapsed)
            }
        }
    }
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    ready: Condvar,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Queued>,
    running: BTreeMap<PathBuf, Instant>,
    closed: bool,
}

impl State {
    fn take_expired(&mut self, now: Instant) -> Vec<Queued> {
        let mut expired = Vec::new();
        for index in (0..self.queue.len()).rev() {
            if self.queue[index].deadline <= now {
                expired.extend(self.queue.remove(index));
            }
        }
        expired
    }

    fn charged(&self, now: Instant) -> impl Iterator<Item = &PathBuf> {
        self.running
            .iter()
            .filter(move |(_, deadline)| **deadline <= now)
            .map(|(root, _)| root)
    }
}

struct Queued {
    root: PathBuf,
    deadline: Instant,
    run: Box<dyn FnOnce(Running) + Send>,
}

struct Running {
    shared: Arc<Shared>,
    root: PathBuf,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().running.remove(&self.root);
    }
}

impl RootIoPool {
    /// Start four dedicated root-I/O workers. Dropping the pool stops idle
    /// workers and discards queued work without joining any stuck worker.
    pub fn new() -> io::Result<Self> {
        Self::with_timeout(ROOT_IO_TIMEOUT)
    }

    pub(super) fn with_timeout(timeout: Duration) -> io::Result<Self> {
        let pool = Self {
            shared: Arc::default(),
            timeout,
        };
        for index in 0..ROOT_IO_WORKERS {
            let shared = pool.shared.clone();
            std::thread::Builder::new()
                .name(format!("subc-root-io-{index}"))
                .spawn(move || worker(shared))?;
        }
        Ok(pool)
    }

    /// Submit one complete operation on a canonical registry root. The
    /// operation's typed result is separate from queue and deadline failures.
    pub(super) fn submit<T: Send + 'static>(
        &self,
        root: PathBuf,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<RootIoJob<T>, RootIoError> {
        let deadline = Instant::now() + self.timeout;
        let (send, result) = oneshot::channel();
        let run = Box::new(move |running| {
            let result = work();
            let in_time = Instant::now() < deadline;
            // Release the reservation before waking a caller that may submit
            // more work on this root. A timed-out wait never releases it.
            drop(running);
            let _ = send.send(if in_time {
                Ok(result)
            } else {
                Err(RootIoError::DeadlineElapsed)
            });
        });
        let expired;
        {
            let mut state = self.shared.state.lock().unwrap();
            let now = Instant::now();
            expired = state.take_expired(now);
            if deadline <= now {
                return Err(RootIoError::DeadlineElapsed);
            }
            if state.running.contains_key(&root) || state.queue.iter().any(|job| job.root == root) {
                return Err(RootIoError::RootBusy);
            }
            if state.charged(now).count() == ROOT_IO_WORKERS {
                return Err(RootIoError::WorkersCharged);
            }
            if state.queue.len() >= ROOT_IO_QUEUE_CAPACITY {
                return Err(RootIoError::QueueFull);
            }
            state.queue.push_back(Queued {
                root,
                deadline,
                run,
            });
        }
        self.shared.ready.notify_one();
        // Drop closures and wake their waiters outside the lock.
        drop(expired);
        Ok(RootIoJob {
            shared: self.shared.clone(),
            deadline,
            result,
        })
    }

    /// After a batch's waits have finished, some of its jobs may still hold
    /// workers. Reserve space for all charged workers before the next batch.
    pub(super) fn batch_slots(&self) -> usize {
        let state = self.shared.state.lock().unwrap();
        // Admission rejects even one job if all four workers are still charged.
        (ROOT_IO_WORKERS - state.charged(Instant::now()).count()).max(1)
    }

    pub(super) fn health(&self) -> Option<ConfigRootIo> {
        let state = self.shared.state.lock().unwrap();
        let mut stuck_roots: Vec<_> = state
            .charged(Instant::now())
            .map(|root| root.to_string_lossy().into_owned())
            .collect();
        stuck_roots.sort_unstable();
        (!stuck_roots.is_empty()).then(|| {
            let mut health = ConfigRootIo::default();
            health.charged_workers = stuck_roots.len() as u64;
            health.stuck_roots = stuck_roots;
            health
        })
    }
}

impl Drop for RootIoPool {
    fn drop(&mut self) {
        let queued = {
            let mut state = self.shared.state.lock().unwrap();
            state.closed = true;
            std::mem::take(&mut state.queue)
        };
        self.shared.ready.notify_all();
        drop(queued);
    }
}

fn worker(shared: Arc<Shared>) {
    loop {
        let (job, running) = {
            let mut state = shared.state.lock().unwrap();
            while state.queue.is_empty() && !state.closed {
                state = shared.ready.wait(state).unwrap();
            }
            let Some(job) = state.queue.pop_front() else {
                return;
            };
            if Instant::now() >= job.deadline {
                drop(state);
                drop(job);
                continue;
            }
            state.running.insert(job.root.clone(), job.deadline);
            let running = Running {
                shared: shared.clone(),
                root: job.root.clone(),
            };
            (job, running)
        };
        // An unwinding operation releases its reservation but doesn't shrink
        // the dedicated pool. No thread is interrupted to enforce a deadline.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (job.run)(running)));
    }
}

#[cfg(test)]
mod tests;
