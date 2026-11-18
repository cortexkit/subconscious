//! Isolate native peer inspection from the async runtime. A native call may
//! wait for a system permission prompt and cannot be cancelled by Rust.

use crate::operator_apps::EVIDENCE_BUDGET as BUDGET;
use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex},
    time::Instant,
};
use tokio::sync::oneshot;

const WORKERS: usize = 2;
const QUEUE: usize = 2;

type Work = Box<dyn FnOnce() + Send>;

struct Job {
    deadline: Instant,
    work: Work,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Job>,
    active: [Option<Instant>; WORKERS],
    closed: bool,
}

type Shared = (Mutex<State>, Condvar);

pub(crate) struct PeerIdentityLane {
    shared: Arc<Shared>,
    #[cfg(test)]
    probe: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for PeerIdentityLane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerIdentityLane").finish_non_exhaustive()
    }
}

impl PeerIdentityLane {
    #[cfg(target_os = "macos")]
    pub(crate) fn native() -> &'static Self {
        static LANE: std::sync::OnceLock<PeerIdentityLane> = std::sync::OnceLock::new();
        LANE.get_or_init(Self::start)
    }

    pub(crate) fn start() -> Self {
        let shared = Arc::new(Shared::default());
        let lane = Self {
            shared,
            #[cfg(test)]
            probe: None,
        };
        for index in 0..WORKERS {
            let shared = lane.shared.clone();
            if std::thread::Builder::new()
                .name(format!("subc-peer-identity-{index}"))
                .spawn(move || worker(&shared, index))
                .is_err()
            {
                lane.close();
                break;
            }
        }
        lane
    }

    #[cfg(test)]
    pub(crate) fn with_probe(mut self, probe: impl Fn() + Send + Sync + 'static) -> Self {
        self.probe = Some(Arc::new(probe));
        self
    }

    pub(crate) async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        self.run_until(Instant::now() + BUDGET, work).await
    }

    pub(crate) async fn run_until<T: Send + 'static>(
        &self,
        deadline: Instant,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        let (tx, rx) = oneshot::channel();
        #[cfg(test)]
        let probe = self.probe.clone();
        let job = Job {
            deadline,
            work: Box::new(move || {
                if tx.is_closed() || Instant::now() >= deadline {
                    return;
                }
                #[cfg(test)]
                if let Some(probe) = probe {
                    probe();
                }
                let result = work();
                if Instant::now() < deadline {
                    let _ = tx.send(result);
                }
            }),
        };
        {
            let (state, ready) = &*self.shared;
            let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
            if state.closed || state.queue.len() >= QUEUE || stuck(&state, now) == WORKERS {
                return None;
            }
            state.queue.push_back(job);
            ready.notify_one();
        }
        let result = tokio::time::timeout_at(deadline.into(), rx)
            .await
            .ok()?
            .ok()?;
        (Instant::now() < deadline).then_some(result)
    }

    #[cfg(test)]
    pub(crate) fn stuck_slots(&self) -> usize {
        let state = self.shared.0.lock().unwrap_or_else(|p| p.into_inner());
        stuck(&state, Instant::now())
    }

    fn close(&self) {
        let (state, ready) = &*self.shared;
        let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
        state.closed = true;
        state.queue.clear();
        ready.notify_all();
    }
}

impl Drop for PeerIdentityLane {
    fn drop(&mut self) {
        // Do not join threads: a native call may never come back.
        self.close();
    }
}

fn stuck(state: &State, now: Instant) -> usize {
    state
        .active
        .iter()
        .flatten()
        .filter(|&&end| end <= now)
        .count()
}

fn worker((state, ready): &Shared, index: usize) {
    loop {
        let job = {
            let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
            state.active[index] = None;
            while state.queue.is_empty() && !state.closed {
                state = ready.wait(state).unwrap_or_else(|p| p.into_inner());
            }
            let Some(job) = state.queue.pop_front() else {
                return;
            };
            state.active[index] = Some(job.deadline);
            job
        };
        (job.work)();
    }
}

#[cfg(test)]
#[path = "operator_apps/lane_tests.rs"]
mod tests;
