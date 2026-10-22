//! Handler-slot pools for data requests.
//!
//! Every data request waits for a handler slot before its handler runs, and
//! keeps the slot until the handler returns. By default all requests share one
//! pool of 64 slots. A module whose requests are sometimes held for minutes can
//! give those requests a pool of their own, so they can never take every slot
//! the module's short requests need. The module declares that pool, and a
//! classifier that sorts requests into pools, with
//! [`ModuleHandler::request_pools`](crate::ModuleHandler::request_pools).

use std::{
    collections::HashMap,
    fmt,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

use crate::{ModuleHandle, RequestKey, DISPATCH_SATURATION_WAIT, HANDLER_TASK_CAPACITY};

/// How long a data request waits for a handler slot before the SDK logs a
/// warning about it. The warning is logged once, while the request is still
/// waiting.
pub(crate) const PERMIT_WAIT_WARNING: Duration = Duration::from_secs(2);

/// The handler-slot pool a data request waits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RequestPool {
    /// The pool every data request uses unless the module's classifier puts
    /// it elsewhere. It has 64 slots.
    Default,
    /// The pool a module reserves for requests that can be held for a long
    /// time, with the limit the module sets in [`RequestPools::with_long_held`].
    LongHeld,
}

impl RequestPool {
    /// The name the SDK uses for this pool in logs and health details.
    pub fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::LongHeld => "long_held",
        }
    }
}

impl fmt::Display for RequestPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A classifier's answer about one data request: the pool it waits in, and
/// an optional label naming the operation, used only in the SDK's logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RequestClass {
    pub pool: RequestPool,
    /// A fixed name for the operation, such as `"research.run"`. It is
    /// `'static` so a label can never carry text taken from the request body.
    pub op: Option<&'static str>,
}

impl RequestClass {
    /// A request for the default pool, with no operation label.
    pub const fn default_pool() -> Self {
        Self {
            pool: RequestPool::Default,
            op: None,
        }
    }

    /// A request for the long-held pool, with no operation label.
    pub const fn long_held() -> Self {
        Self {
            pool: RequestPool::LongHeld,
            op: None,
        }
    }

    /// The same class, labelled with the operation name `op`.
    pub const fn with_op(mut self, op: &'static str) -> Self {
        self.op = Some(op);
        self
    }
}

impl Default for RequestClass {
    fn default() -> Self {
        Self::default_pool()
    }
}

type Classifier = dyn Fn(&[u8]) -> RequestClass + Send + Sync;

/// How a module's data requests share handler slots, returned from
/// [`ModuleHandler::request_pools`](crate::ModuleHandler::request_pools).
///
/// [`RequestPools::new`] is the behaviour a module gets when it declares
/// nothing: one pool of 64 slots for every data request.
/// [`RequestPools::with_long_held`] adds a second pool and the classifier
/// that decides which requests use it.
#[derive(Clone, Default)]
#[non_exhaustive]
pub struct RequestPools {
    long_held: Option<(usize, Arc<Classifier>)>,
}

impl RequestPools {
    /// One pool of 64 slots for every data request, with no classifier.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a long-held pool of `limit` slots, and the classifier that puts
    /// each data request in a pool.
    ///
    /// The SDK calls `classifier` with the request's raw body, before the
    /// request takes a slot; the SDK itself never interprets a body. A
    /// request the classifier puts in [`RequestPool::LongHeld`] waits only
    /// for a long-held slot, and every other request waits only for a
    /// default slot, so a full pool never delays requests of the other one.
    ///
    /// The classifier runs on the task that reads the connection's frames,
    /// so it must be cheap and must not block: peek at the operation name,
    /// do not decode the whole body. If it panics, the request uses the
    /// default pool. A `limit` of 0 is raised to 1, so long-held requests
    /// are delayed, never stranded.
    pub fn with_long_held<F>(mut self, limit: usize, classifier: F) -> Self
    where
        F: Fn(&[u8]) -> RequestClass + Send + Sync + 'static,
    {
        self.long_held = Some((limit.max(1), Arc::new(classifier)));
        self
    }

    /// The long-held pool's slot limit, or `None` when there is no long-held
    /// pool.
    pub fn long_held_limit(&self) -> Option<usize> {
        self.long_held.as_ref().map(|(limit, _)| *limit)
    }
}

impl fmt::Debug for RequestPools {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestPools")
            .field("long_held_limit", &self.long_held_limit())
            .finish_non_exhaustive()
    }
}

/// Figures for one handler-slot pool at one moment, from
/// [`ModuleHandle::request_pool_snapshots`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RequestPoolSnapshot {
    pub pool: RequestPool,
    /// Slots held by requests whose handlers are running.
    pub in_use: usize,
    /// The pool's number of slots.
    pub limit: usize,
    /// Requests received and waiting for a slot in this pool.
    pub waiting: usize,
    /// How long the request that has waited longest has been waiting, or
    /// `None` when no request is waiting.
    pub oldest_wait: Option<Duration>,
}

type WaitingRequests = Arc<Mutex<HashMap<RequestKey, Instant>>>;

/// One pool's slots and the requests waiting for them, shared by the
/// connection's dispatcher, its request tasks and its [`ModuleHandle`].
#[derive(Clone)]
pub(crate) struct PermitPool {
    pub(crate) pool: RequestPool,
    pub(crate) limit: usize,
    pub(crate) permits: Arc<Semaphore>,
    /// When each request in this pool started waiting for a slot. Entries are
    /// added before the request task is spawned and removed once it has a slot
    /// or has stopped waiting. The lock is never held over an await or handler
    /// work, so health and snapshots can read it without queueing behind the
    /// work they measure.
    pub(crate) waiting: WaitingRequests,
}

impl PermitPool {
    pub(crate) fn new(pool: RequestPool, limit: usize) -> Self {
        Self {
            pool,
            limit,
            permits: Arc::new(Semaphore::new(limit)),
            waiting: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn in_use(&self) -> usize {
        self.limit.saturating_sub(self.permits.available_permits())
    }

    fn lock_waiting(&self) -> std::sync::MutexGuard<'_, HashMap<RequestKey, Instant>> {
        self.waiting
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Record that the request `key` has started waiting for a slot in this
    /// pool. The record is removed when the returned guard drops.
    pub(crate) fn start_waiting(&self, key: RequestKey) -> PermitWait {
        let since = Instant::now();
        self.lock_waiting().insert(key, since);
        PermitWait {
            waiting: Arc::clone(&self.waiting),
            key,
            since,
        }
    }

    pub(crate) fn snapshot(&self) -> RequestPoolSnapshot {
        let waiting = self.lock_waiting();
        let oldest_wait = waiting.values().min().map(Instant::elapsed);
        let count = waiting.len();
        drop(waiting);
        RequestPoolSnapshot {
            pool: self.pool,
            in_use: self.in_use(),
            limit: self.limit,
            waiting: count,
            oldest_wait,
        }
    }

    /// The saturation sentence for health, when every slot is in use and a
    /// request has waited longer than [`DISPATCH_SATURATION_WAIT`] for one.
    /// `named` adds the pool's name, which a module with a single pool never
    /// gets, so its wording is unchanged from before pools existed.
    fn saturation(&self, named: bool) -> Option<String> {
        let snapshot = self.snapshot();
        let age = snapshot.oldest_wait.filter(|age| {
            self.permits.available_permits() == 0 && *age > DISPATCH_SATURATION_WAIT
        })?;
        let pool = if named {
            format!(" in the {} pool", self.pool.name())
        } else {
            String::new()
        };
        Some(format!(
            "request dispatch saturated{pool}: {}/{} in use, oldest waiting {:.1} s",
            snapshot.in_use,
            snapshot.limit,
            age.as_secs_f64(),
        ))
    }
}

/// Holds a request's entry in its pool's `waiting` map and removes it on drop,
/// so the entry goes even if the waiting task exits on connection close or
/// panics.
pub(crate) struct PermitWait {
    waiting: WaitingRequests,
    key: RequestKey,
    since: Instant,
}

impl Drop for PermitWait {
    fn drop(&mut self) {
        self.waiting
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.key);
    }
}

/// The pools of one connection and the module's classifier.
pub(crate) struct DispatchPools {
    pub(crate) default: PermitPool,
    /// Present exactly when the module declared a classifier.
    pub(crate) long_held: Option<(PermitPool, Arc<Classifier>)>,
}

impl DispatchPools {
    pub(crate) fn new(config: RequestPools) -> Self {
        Self::with_default_limit(HANDLER_TASK_CAPACITY, config)
    }

    pub(crate) fn with_default_limit(default_limit: usize, config: RequestPools) -> Self {
        Self {
            default: PermitPool::new(RequestPool::Default, default_limit),
            long_held: config.long_held.map(|(limit, classifier)| {
                (PermitPool::new(RequestPool::LongHeld, limit), classifier)
            }),
        }
    }

    /// Every pool, the default pool first.
    pub(crate) fn pools(&self) -> impl Iterator<Item = &PermitPool> {
        std::iter::once(&self.default).chain(self.long_held.as_ref().map(|(pool, _)| pool))
    }

    /// The pool a request with `body` waits in, and its operation label. With
    /// no classifier every request uses the default pool and has no label.
    pub(crate) fn classify(&self, body: &[u8]) -> (&PermitPool, Option<&'static str>) {
        let Some((long_held, classifier)) = &self.long_held else {
            return (&self.default, None);
        };
        let class = match catch_unwind(AssertUnwindSafe(|| classifier(body))) {
            Ok(class) => class,
            Err(_) => {
                tracing::warn!(
                    "the module's request classifier panicked; the request uses the default pool"
                );
                RequestClass::default_pool()
            }
        };
        let pool = match class.pool {
            RequestPool::LongHeld => long_held,
            RequestPool::Default => &self.default,
        };
        (pool, class.op)
    }

    /// The saturation sentences of every saturated pool, joined, for health.
    /// A module with one pool gets the same wording as before pools existed;
    /// with two, each sentence names its pool.
    pub(crate) fn saturation(&self) -> Option<String> {
        let named = self.long_held.is_some();
        let sentences = self
            .pools()
            .filter_map(|pool| pool.saturation(named))
            .collect::<Vec<_>>();
        (!sentences.is_empty()).then(|| sentences.join("; "))
    }

    pub(crate) fn snapshots(&self) -> Vec<RequestPoolSnapshot> {
        self.pools().map(PermitPool::snapshot).collect()
    }

    /// Close every pool, so requests still waiting for a slot stop waiting.
    pub(crate) fn close(&self) {
        for pool in self.pools() {
            pool.permits.close();
        }
    }
}

/// How a request's wait for a handler slot ended.
pub(crate) enum PermitWaitOutcome {
    Acquired(OwnedSemaphorePermit),
    /// The request was cancelled before it got a slot.
    Cancelled,
    /// The pool was closed because the connection ended.
    Closed,
}

/// Where a waiting request came from, for the slow-wait warning.
pub(crate) struct WaitingRequest {
    pub(crate) op: Option<&'static str>,
    pub(crate) channel: u16,
    pub(crate) corr: u64,
}

/// Wait for a slot in `pool`, stopping at once if `cancelled` fires. Once the
/// request has waited [`PERMIT_WAIT_WARNING`] and is still waiting, log one
/// warning naming its operation label, pool, the pool's figures, and its route
/// channel and correlation id. The request body is never logged.
pub(crate) async fn wait_for_permit(
    pool: &PermitPool,
    wait: PermitWait,
    request: WaitingRequest,
    cancelled: &CancellationToken,
) -> PermitWaitOutcome {
    let acquire = Arc::clone(&pool.permits).acquire_owned();
    let warn_at = tokio::time::sleep_until(wait.since + PERMIT_WAIT_WARNING);
    tokio::pin!(acquire, warn_at);
    let mut warned = false;
    let outcome = loop {
        tokio::select! {
            // A slot is taken in preference to the other two. Connection
            // teardown closes the pools before it cancels requests, so a
            // waiting request then sees the closed pool and ends without
            // answering, as it did before cancellation was watched here.
            biased;
            permit = &mut acquire => {
                break match permit {
                    Ok(permit) => PermitWaitOutcome::Acquired(permit),
                    Err(_) => PermitWaitOutcome::Closed,
                };
            }
            () = cancelled.cancelled() => break PermitWaitOutcome::Cancelled,
            // A finished sleep stays ready, so `warned` is what limits the
            // warning to once.
            () = &mut warn_at, if !warned => {
                warned = true;
                tracing::warn!(
                    op = request.op.unwrap_or("<none>"),
                    pool = pool.pool.name(),
                    in_use = pool.in_use(),
                    limit = pool.limit,
                    channel = request.channel,
                    corr = request.corr,
                    waited_ms = u64::try_from(wait.since.elapsed().as_millis()).unwrap_or(u64::MAX),
                    "data request is still waiting for a handler slot after {} s",
                    PERMIT_WAIT_WARNING.as_secs(),
                );
            }
        }
    };
    drop(wait);
    outcome
}

impl ModuleHandle {
    /// Figures for each handler-slot pool of this connection, the default
    /// pool first, then the long-held pool when the module declared one in
    /// [`ModuleHandler::request_pools`](crate::ModuleHandler::request_pools).
    pub fn request_pool_snapshots(&self) -> Vec<RequestPoolSnapshot> {
        self.dispatch_pools().snapshots()
    }

    /// Replace this connection's pools with the ones `config` declares.
    /// Called once, before the connection's dispatcher exists.
    pub(crate) fn install_dispatch_pools(&self, config: RequestPools) {
        *self
            .shared
            .dispatch_pools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Arc::new(DispatchPools::new(config));
    }

    pub(crate) fn dispatch_pools(&self) -> Arc<DispatchPools> {
        Arc::clone(
            &self
                .shared
                .dispatch_pools
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }
}
