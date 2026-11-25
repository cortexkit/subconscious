//! Dedicated workers for filesystem access to registry roots. The queue lock
//! protects bookkeeping only; no filesystem work runs while it is held.

use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use subc_control::config::ConfigRootIo;
use tokio::sync::{oneshot, watch};

const ROOT_IO_WORKERS: usize = 4;
const ROOT_IO_QUEUE_CAPACITY: usize = 16;
const ROOT_IO_TIMEOUT: Duration = Duration::from_secs(10);

/// A daemon's lane for operations on potentially unresponsive registry roots.
pub struct RootIoPool {
    shared: Arc<Shared>,
    timeout: Duration,
    #[cfg(test)]
    pub(super) before_work: Mutex<Option<RootWorkHook>>,
}

#[cfg(test)]
type RootWorkHook = Arc<dyn Fn(&std::path::Path) + Send + Sync>;

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
    running: BTreeMap<PathBuf, Reservation>,
    closed: bool,
}

impl State {
    fn take_expired(&mut self, now: Instant) -> Vec<Queued> {
        let mut expired = Vec::new();
        for index in (0..self.queue.len()).rev() {
            if self.queue[index].reservation.deadline <= now {
                expired.extend(self.queue.remove(index));
            }
        }
        expired
    }

    fn charged(&self, now: Instant) -> impl Iterator<Item = &PathBuf> {
        self.running
            .iter()
            .filter(move |(_, reservation)| reservation.deadline <= now)
            .map(|(root, _)| root)
    }
}

struct Queued {
    root: PathBuf,
    reservation: Reservation,
    run: Box<dyn FnOnce(Running) + Send>,
}

struct Reservation {
    deadline: Instant,
    // Dropping the sender wakes only readers waiting on this reservation.
    release: Option<watch::Sender<()>>,
}

impl Reservation {
    fn released(&mut self) -> watch::Receiver<()> {
        self.release
            .get_or_insert_with(|| watch::Sender::new(()))
            .subscribe()
    }
}

struct Running {
    shared: Arc<Shared>,
    root: PathBuf,
}

impl Drop for Running {
    fn drop(&mut self) {
        let reservation = self.shared.state.lock().unwrap().running.remove(&self.root);
        drop(reservation);
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
            #[cfg(test)]
            before_work: Mutex::default(),
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
        let (queued, job) = self.prepare(root, self.deadline(), work);
        self.enqueue(queued).map_err(|(error, _)| error)?;
        Ok(job)
    }

    /// Read root evidence from a blocking caller. As with `RootIoJob::wait`,
    /// success means the reservation has been released before returning.
    pub(super) fn read_blocking<T: Send + 'static>(
        &self,
        root: PathBuf,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, RootIoError> {
        let deadline = self.deadline();
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let queued = self.prepare_with_completion(root, deadline, work, move |result| {
            let _ = send.send(result);
        });
        self.enqueue(queued).map_err(|(error, _)| error)?;
        match receive.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) if Instant::now() < deadline => {
                Err(RootIoError::WorkerStopped)
            }
            Err(_) => {
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

    pub(super) fn deadline(&self) -> Instant {
        Instant::now() + self.timeout
    }

    /// Try to submit now so other roots in the same batch can run in parallel.
    /// An interactive read can retry a busy root without occupying the queue or
    /// a worker. All retries use the original deadline.
    pub(super) fn start_read<T: Send + 'static>(
        &self,
        root: PathBuf,
        wait_until: Option<Instant>,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> impl Future<Output = Result<T, RootIoError>> + Send + '_ {
        let deadline = wait_until.unwrap_or_else(|| self.deadline());
        let (queued, job) = self.prepare(root, deadline, work);
        let mut admission = self.enqueue(queued);
        async move {
            loop {
                match admission {
                    Ok(()) => return job.wait().await,
                    Err((RootIoError::RootBusy, queued)) if wait_until.is_some() => {
                        self.wait_for_release(&queued.root, deadline).await;
                        admission = self.enqueue(queued);
                    }
                    Err((error, _)) => return Err(error),
                }
            }
        }
    }

    async fn wait_for_release(&self, root: &Path, deadline: Instant) {
        let (mut released, until) = {
            let mut state = self.shared.state.lock().unwrap();
            if let Some(running) = state.running.get_mut(root) {
                (running.released(), deadline)
            } else if let Some(job) = state.queue.iter_mut().find(|job| job.root == root) {
                // Expired queued work no longer reserves the root, even if no
                // worker or caller has removed it from the queue yet.
                (
                    job.reservation.released(),
                    deadline.min(job.reservation.deadline),
                )
            } else {
                return;
            }
        };
        let _ = tokio::time::timeout_at(until.into(), released.changed()).await;
    }

    fn prepare<T: Send + 'static>(
        &self,
        root: PathBuf,
        deadline: Instant,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> (Queued, RootIoJob<T>) {
        let (send, result) = oneshot::channel();
        let queued = self.prepare_with_completion(root, deadline, work, move |finished| {
            let _ = send.send(finished);
        });
        (
            queued,
            RootIoJob {
                shared: self.shared.clone(),
                deadline,
                result,
            },
        )
    }

    fn prepare_with_completion<T: Send + 'static>(
        &self,
        root: PathBuf,
        deadline: Instant,
        work: impl FnOnce() -> T + Send + 'static,
        finish: impl FnOnce(Result<T, RootIoError>) + Send + 'static,
    ) -> Queued {
        #[cfg(test)]
        let before = self
            .before_work
            .lock()
            .unwrap()
            .clone()
            .map(|hook| (hook, root.clone()));
        let run = Box::new(move |running| {
            #[cfg(test)]
            if let Some((hook, root)) = before {
                hook(&root);
            }
            let result = work();
            let in_time = Instant::now() < deadline;
            // Release the reservation before waking a caller that may submit
            // more work on this root. A timed-out wait never releases it.
            drop(running);
            finish(if in_time {
                Ok(result)
            } else {
                Err(RootIoError::DeadlineElapsed)
            });
        });
        Queued {
            root,
            reservation: Reservation {
                deadline,
                release: None,
            },
            run,
        }
    }

    fn enqueue(&self, job: Queued) -> Result<(), (RootIoError, Queued)> {
        let expired;
        {
            let mut state = self.shared.state.lock().unwrap();
            let now = Instant::now();
            expired = state.take_expired(now);
            if job.reservation.deadline <= now {
                return Err((RootIoError::DeadlineElapsed, job));
            }
            if state.running.contains_key(&job.root)
                || state.queue.iter().any(|queued| queued.root == job.root)
            {
                return Err((RootIoError::RootBusy, job));
            }
            if state.charged(now).count() == ROOT_IO_WORKERS {
                return Err((RootIoError::WorkersCharged, job));
            }
            if state.queue.len() >= ROOT_IO_QUEUE_CAPACITY {
                return Err((RootIoError::QueueFull, job));
            }
            state.queue.push_back(job);
        }
        self.shared.ready.notify_one();
        // Drop closures and wake their waiters outside the lock.
        drop(expired);
        Ok(())
    }

    /// After a batch's waits have finished, some of its jobs may still hold
    /// workers. Reserve space for all charged workers before the next batch.
    pub(super) fn batch_slots(&self) -> usize {
        let state = self.shared.state.lock().unwrap();
        // Admission rejects even one job if all four workers are still charged.
        (ROOT_IO_WORKERS - state.charged(Instant::now()).count()).max(1)
    }

    pub(super) fn is_busy(&self, root: &Path) -> bool {
        let state = self.shared.state.lock().unwrap();
        state.running.contains_key(root)
            || state
                .queue
                .iter()
                .any(|job| job.root == root && job.reservation.deadline > Instant::now())
    }

    pub(super) fn is_charged(&self, root: &Path) -> bool {
        self.shared
            .state
            .lock()
            .unwrap()
            .running
            .get(root)
            .is_some_and(|reservation| reservation.deadline <= Instant::now())
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
        let (run, running) = {
            let mut state = shared.state.lock().unwrap();
            while state.queue.is_empty() && !state.closed {
                state = shared.ready.wait(state).unwrap();
            }
            let Some(job) = state.queue.pop_front() else {
                return;
            };
            if Instant::now() >= job.reservation.deadline {
                drop(state);
                drop(job);
                continue;
            }
            state.running.insert(job.root.clone(), job.reservation);
            let running = Running {
                shared: shared.clone(),
                root: job.root,
            };
            (job.run, running)
        };
        // An unwinding operation releases its reservation but doesn't shrink
        // the dedicated pool. No thread is interrupted to enforce a deadline.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(running)));
    }
}

#[cfg(test)]
mod tests;
