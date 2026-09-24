//! The sentinel probe and the module health answer.
//!
//! The sentinel proves the bus carries a message end to end. Once bootstrap has
//! connected ck-bus's box-account user, the sentinel subscribes that same connection
//! to its sentinel subject (`AccountNames::sentinel_ping`) as the responder, and once
//! per period sends a request there as the requester. The reply comes back through the
//! server on the connection's own inbox (`_INBOX.<box user>`), so one round trip covers
//! the socket, the server, the user's permissions and both directions of delivery. A
//! round then reads ck-bus's own census key, because revocation and reconciliation
//! cannot act while the census is unreadable, and the health answer names that cause.
//!
//! The verdict:
//! - A process starts down with class `Unavailable` and says so until its own first
//!   round answers. The verdict an earlier process persisted is logged at start and
//!   never used.
//! - A round that got its reply within the timeout (and read the census) is a success,
//!   and the verdict is up.
//! - While up, a failed round is tolerated twice; the third consecutive failure reports
//!   down. While down, every failed round keeps the verdict down with its own class and
//!   cause. So a server that stops answering is reported down no later than
//!   `3 * period + timeout` after it stopped.
//! - A permissions violation the server reports on the sentinel subject or on the
//!   user's inbox during a round makes that round's class `Denied`, even when the
//!   requester also timed out.
//! - Up is only ever the result of a measured success: when the last success is older
//!   than `3 * period + timeout` (the same bound a failing server is reported within),
//!   the answer is down with cause `sentinel-stale`, whatever the probing task is doing.
//!   That covers a probing task that is itself stuck.
//!
//! The health answer reads the verdict from memory under a lock the probing task holds
//! only to record a result, so it never waits on the bus. Until bootstrap is ready, the
//! answer is bootstrap's own, naming its cause.
//!
//! Each change of verdict is logged (`ckbus.sentinel.verdict`), written to
//! `sentinel_verdict.json` and published on the sentinel subject.

pub mod verdict;

use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use cortexkit_bus_naming::AccountNames;
use futures_util::StreamExt;
use serde_json::{json, Value};
use subc_protocol::session::{HealthReport, HealthStatus};
use tokio::{
    sync::{broadcast, watch},
    time::Instant,
};

use crate::{
    bootstrap::{
        self,
        plane::{BoxPlane, SentinelLink},
        Ready,
    },
    grants,
    runtime::{SeamResult, SentinelHealth},
};
use verdict::{VerdictRead, VerdictStore};

/// Causes named in health `metrics.cause` while the sentinel reports down.
pub mod cause {
    /// This process has not finished a probe round yet.
    pub const NO_ANSWER_YET: &str = "sentinel-no-answer-yet";
    /// The request failed or got no reply within the timeout.
    pub const NO_REPLY: &str = "sentinel-no-reply";
    /// A reply came back, but not the one this round sent.
    pub const REPLY_MISMATCH: &str = "sentinel-reply-mismatch";
    /// The server reported a permissions violation on the sentinel subject or inbox.
    pub const DENIED: &str = "sentinel-permission-denied";
    /// The responder could not subscribe to the sentinel subject.
    pub const RESPONDER_UNAVAILABLE: &str = "sentinel-responder-unavailable";
    /// The box plane offers no connection to probe on.
    pub const LINK_ABSENT: &str = "sentinel-link-absent";
    /// ck-bus's own census key could not be read.
    pub const CENSUS_READ_FAILED: &str = "census-read-failed";
    /// The last success is older than the down bound.
    pub const STALE: &str = "sentinel-stale";
}

/// How many consecutive failed rounds turn an up verdict down.
pub const FAILURES_TO_DOWN: u32 = 3;

/// Extra time a round gets, beyond its timeout, to classify its own failure before the
/// driving loop gives up on it. A round that ignores its timeout altogether is cut off
/// here and counted as a round with no reply.
const ROUND_GUARD_GRACE: Duration = Duration::from_millis(100);

/// The class written at `metrics.class` while down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Unavailable,
    Denied,
}

impl Class {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "Unavailable",
            Self::Denied => "Denied",
        }
    }
}

/// The probe's period and timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub period: Duration,
    pub timeout: Duration,
}

impl Timing {
    /// `3 * period + timeout`: how long after a server stops answering the verdict is
    /// down, and how old a success may be and still count.
    pub fn down_bound(&self) -> Duration {
        self.period * FAILURES_TO_DOWN + self.timeout
    }
}

/// What one probe round measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoundOutcome {
    /// The reply to this round's request, delivered on `reply_subject`.
    Answered { reply_subject: String },
    Failed {
        class: Class,
        cause: &'static str,
        message: String,
    },
}

impl RoundOutcome {
    fn failed(class: Class, cause: &'static str, message: impl Into<String>) -> Self {
        Self::Failed {
            class,
            cause,
            message: message.into(),
        }
    }
}

/// One probe round trip. `round` numbers the request so a late reply to an earlier
/// round is never taken for this one's.
#[async_trait]
pub trait Probe: Send + Sync {
    async fn round(&self, round: u64, timeout: Duration) -> RoundOutcome;
}

/// The sentinel's current verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Up {
        reply_subject: String,
    },
    Down {
        class: Class,
        cause: &'static str,
        message: String,
    },
}

impl Verdict {
    /// Whether `other` is a different verdict for the log and the verdict file. A
    /// changed message alone is not a change.
    fn differs(&self, other: &Verdict) -> bool {
        match (self, other) {
            (Self::Up { .. }, Self::Up { .. }) => false,
            (
                Self::Down { class, cause, .. },
                Self::Down {
                    class: other_class,
                    cause: other_cause,
                    ..
                },
            ) => class != other_class || cause != other_cause,
            _ => true,
        }
    }
}

struct State {
    verdict: Verdict,
    last_success: Option<Instant>,
    consecutive_failures: u32,
    rounds: u64,
}

/// The verdict this process measured, shared by the probing task (which records) and
/// the health answer (which reads).
pub struct Monitor {
    state: Mutex<State>,
    timing: Timing,
    incarnation: String,
}

impl Monitor {
    pub fn new(incarnation: String, timing: Timing) -> Self {
        Self {
            state: Mutex::new(State {
                verdict: Verdict::Down {
                    class: Class::Unavailable,
                    cause: cause::NO_ANSWER_YET,
                    message: "this process has not finished a sentinel round yet".to_string(),
                },
                last_success: None,
                consecutive_failures: 0,
                rounds: 0,
            }),
            timing,
            incarnation,
        }
    }

    pub fn timing(&self) -> Timing {
        self.timing
    }

    pub fn incarnation(&self) -> &str {
        &self.incarnation
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Records one round. Returns the new verdict when it changed.
    pub fn record(&self, outcome: RoundOutcome) -> Option<Verdict> {
        let mut state = self.state();
        state.rounds += 1;
        let next = match outcome {
            RoundOutcome::Answered { reply_subject } => {
                state.consecutive_failures = 0;
                state.last_success = Some(Instant::now());
                Verdict::Up { reply_subject }
            }
            RoundOutcome::Failed {
                class,
                cause,
                message,
            } => {
                state.consecutive_failures += 1;
                let tolerated = matches!(state.verdict, Verdict::Up { .. })
                    && state.consecutive_failures < FAILURES_TO_DOWN;
                if tolerated {
                    state.verdict.clone()
                } else {
                    Verdict::Down {
                        class,
                        cause,
                        message,
                    }
                }
            }
        };
        let changed = state.verdict.differs(&next);
        state.verdict = next;
        changed.then(|| state.verdict.clone())
    }

    /// The health answer. Reads memory only.
    pub fn report(&self) -> HealthReport {
        let state = self.state();
        let bound = self.timing.down_bound();
        let age = state.last_success.map(|at| at.elapsed());
        let common = json!({
            "incarnation": self.incarnation,
            "rounds": state.rounds,
            "consecutive_failures": state.consecutive_failures,
            "last_success_age_ms": age.map(|age| age.as_millis() as u64),
            "down_bound_ms": bound.as_millis() as u64,
        });
        let verdict = match (&state.verdict, age) {
            (Verdict::Up { .. }, Some(age)) if age > bound => Verdict::Down {
                class: Class::Unavailable,
                cause: cause::STALE,
                message: format!(
                    "the last successful round trip was {} ms ago, older than 3 * period + \
                     timeout ({} ms)",
                    age.as_millis(),
                    bound.as_millis()
                ),
            },
            (verdict, _) => verdict.clone(),
        };
        drop(state);
        let mut metrics = common;
        match verdict {
            Verdict::Up { reply_subject } => {
                metrics["reply_subject"] = Value::String(reply_subject);
                HealthReport {
                    status: HealthStatus::Ok,
                    detail: Some("bus.health.up".to_string()),
                    metrics: Some(metrics),
                }
            }
            Verdict::Down {
                class,
                cause,
                message,
            } => {
                metrics["class"] = Value::String(class.as_str().to_string());
                metrics["cause"] = Value::String(cause.to_string());
                metrics["message"] = Value::String(message);
                HealthReport {
                    status: HealthStatus::Failing,
                    detail: Some("bus.health.down".to_string()),
                    metrics: Some(metrics),
                }
            }
        }
    }

    /// Runs one round, bounded by the timeout plus `ROUND_GUARD_GRACE`.
    pub async fn run_round(&self, probe: &dyn Probe, round: u64) -> Option<Verdict> {
        let timeout = self.timing.timeout;
        let outcome =
            tokio::time::timeout(timeout + ROUND_GUARD_GRACE, probe.round(round, timeout))
                .await
                .unwrap_or_else(|_| {
                    RoundOutcome::failed(
                        Class::Unavailable,
                        cause::NO_REPLY,
                        format!("the round did not finish within {} ms", timeout.as_millis()),
                    )
                });
        self.record(outcome)
    }

    /// Probes once per period, forever, calling `changed` with each new verdict.
    pub async fn drive(&self, probe: &dyn Probe, changed: &(dyn Fn(&Verdict) + Send + Sync)) {
        let mut ticks = tokio::time::interval(self.timing.period);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut round = 0u64;
        loop {
            ticks.tick().await;
            round += 1;
            if let Some(verdict) = self.run_round(probe, round).await {
                changed(&verdict);
            }
        }
    }
}

/// Whether a server error line is a permissions violation on the sentinel's own
/// subjects: the sentinel subject, or the user's inbox.
pub fn is_own_violation(line: &str, sentinel_subject: &str, user_public: &str) -> bool {
    line.to_ascii_lowercase().contains("permissions violation")
        && (line.contains(sentinel_subject) || line.contains(&format!("_INBOX.{user_public}")))
}

/// The probe over ck-bus's box-account connection.
pub struct NatsProbe {
    link: SentinelLink,
    subject: String,
    box_plane: Arc<dyn BoxPlane>,
    names: AccountNames,
    census_key: String,
    incarnation: String,
    /// Server error lines since the previous round. Opened before the responder
    /// subscribes, so a subscription the server refuses is seen by the first round.
    errors: tokio::sync::Mutex<broadcast::Receiver<String>>,
    responder: tokio::task::JoinHandle<()>,
}

impl Drop for NatsProbe {
    fn drop(&mut self) {
        self.responder.abort();
    }
}

impl NatsProbe {
    /// Subscribes the responder: every request on the sentinel subject is answered with
    /// its own payload, to its reply subject.
    pub async fn start(
        link: SentinelLink,
        box_plane: Arc<dyn BoxPlane>,
        names: AccountNames,
        module_id: &str,
        incarnation: &str,
    ) -> Result<Self, RoundOutcome> {
        let census_key = AccountNames::census_key(module_id).map_err(|error| {
            RoundOutcome::failed(
                Class::Unavailable,
                cause::CENSUS_READ_FAILED,
                error.to_string(),
            )
        })?;
        let subject = names.sentinel_ping();
        let errors = link.server_errors.subscribe();
        let unavailable = |error: String| {
            RoundOutcome::failed(Class::Unavailable, cause::RESPONDER_UNAVAILABLE, error)
        };
        let mut requests = link
            .client
            .subscribe(subject.clone())
            .await
            .map_err(|error| unavailable(format!("subscribe {subject}: {error}")))?;
        link.client
            .flush()
            .await
            .map_err(|error| unavailable(format!("flush after subscribing {subject}: {error}")))?;
        let client = link.client.clone();
        let responder = tokio::spawn(async move {
            while let Some(request) = requests.next().await {
                // A publish with no reply subject (bootstrap's ready line, a published
                // verdict) is not a probe.
                if let Some(reply) = request.reply {
                    let _ = client.publish(reply, request.payload).await;
                }
            }
        });
        Ok(Self {
            link,
            subject,
            box_plane,
            names,
            census_key,
            incarnation: incarnation.to_string(),
            errors: tokio::sync::Mutex::new(errors),
            responder,
        })
    }

    /// The first own-subject violation among the lines already received, if any.
    fn pending_violation(&self, errors: &mut broadcast::Receiver<String>) -> Option<String> {
        loop {
            match errors.try_recv() {
                Ok(line) if is_own_violation(&line, &self.subject, &self.link.user_public) => {
                    return Some(line)
                }
                Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => return None,
            }
        }
    }

    /// Waits for the next own-subject violation.
    async fn next_violation(&self, errors: &mut broadcast::Receiver<String>) -> String {
        loop {
            match errors.recv().await {
                Ok(line) if is_own_violation(&line, &self.subject, &self.link.user_public) => {
                    return line
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => {
                    std::future::pending::<()>().await;
                }
            }
        }
    }

    fn denied(line: String) -> RoundOutcome {
        RoundOutcome::failed(Class::Denied, cause::DENIED, line)
    }

    /// Publishes a changed verdict on the sentinel subject, without waiting on a server
    /// that may be down.
    pub async fn publish_verdict(&self, verdict: &Value, timeout: Duration) {
        let payload = verdict.to_string();
        let _ = tokio::time::timeout(
            timeout,
            self.link
                .client
                .publish(self.subject.clone(), payload.into_bytes().into()),
        )
        .await;
    }
}

#[async_trait]
impl Probe for NatsProbe {
    async fn round(&self, round: u64, timeout: Duration) -> RoundOutcome {
        let started = Instant::now();
        let mut errors = self.errors.lock().await;
        if let Some(line) = self.pending_violation(&mut errors) {
            return Self::denied(line);
        }
        let payload = json!({ "incarnation": self.incarnation, "round": round }).to_string();
        let request = self
            .link
            .client
            .request(self.subject.clone(), payload.clone().into_bytes().into());
        let reply = tokio::select! {
            reply = tokio::time::timeout(timeout, request) => reply,
            line = self.next_violation(&mut errors) => return Self::denied(line),
        };
        let reply = match reply {
            Ok(Ok(reply)) => reply,
            failed => {
                // A violation reported during the round wins over the timeout.
                if let Some(line) = self.pending_violation(&mut errors) {
                    return Self::denied(line);
                }
                let message = match failed {
                    Ok(Err(error)) => format!("request on {}: {error}", self.subject),
                    _ => format!(
                        "no reply on {} within {} ms",
                        self.subject,
                        timeout.as_millis()
                    ),
                };
                return RoundOutcome::failed(Class::Unavailable, cause::NO_REPLY, message);
            }
        };
        if reply.payload.as_ref() != payload.as_bytes() {
            return RoundOutcome::failed(
                Class::Unavailable,
                cause::REPLY_MISMATCH,
                format!(
                    "round {round} got {:?}",
                    String::from_utf8_lossy(&reply.payload)
                ),
            );
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        match tokio::time::timeout(
            remaining,
            self.box_plane.census_get(&self.names, &self.census_key),
        )
        .await
        {
            Ok(Ok(_)) => RoundOutcome::Answered {
                reply_subject: reply.subject.to_string(),
            },
            Ok(Err(error)) => RoundOutcome::failed(
                Class::Unavailable,
                cause::CENSUS_READ_FAILED,
                format!("census key {}: {}", self.census_key, error.message),
            ),
            Err(_) => RoundOutcome::failed(
                Class::Unavailable,
                cause::CENSUS_READ_FAILED,
                format!(
                    "census key {} not read within the {} ms round",
                    self.census_key,
                    timeout.as_millis()
                ),
            ),
        }
    }
}

/// The health answer `health.check` serves: bootstrap's while bootstrap is not ready,
/// the sentinel's verdict after.
pub struct Answer {
    bootstrap: Arc<dyn SentinelHealth>,
    ready: watch::Receiver<Option<Arc<Ready>>>,
    monitor: Arc<Monitor>,
}

impl Answer {
    pub fn new(
        bootstrap: Arc<dyn SentinelHealth>,
        ready: watch::Receiver<Option<Arc<Ready>>>,
        monitor: Arc<Monitor>,
    ) -> Self {
        Self {
            bootstrap,
            ready,
            monitor,
        }
    }
}

#[async_trait]
impl SentinelHealth for Answer {
    async fn report_health(&self) -> SeamResult<HealthReport> {
        if self.ready.borrow().is_some() {
            return Ok(self.monitor.report());
        }
        let answer = self.bootstrap.report_health().await?;
        // Bootstrap finished but has not yet published its connections: from here on
        // the sentinel answers, and it starts down until its first round.
        let finished = answer
            .metrics
            .as_ref()
            .is_some_and(|metrics| metrics["cause"] == bootstrap::cause::SENTINEL_NOT_LANDED);
        if finished {
            return Ok(self.monitor.report());
        }
        Ok(answer)
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// One structured stderr line, with `at_ms` so a reader can time transitions from the
/// log alone.
fn log_event(event: &str, fields: Value) {
    let mut line = json!({ "event": event, "at_ms": unix_ms() });
    if let (Some(line), Value::Object(fields)) = (line.as_object_mut(), fields) {
        line.extend(fields);
    }
    eprintln!("{line}");
}

/// A verdict as it is logged, persisted and published.
pub fn verdict_record(verdict: &Verdict, incarnation: &str) -> Value {
    let mut record = json!({
        "event": "ckbus.sentinel.verdict",
        "incarnation": incarnation,
        "at_ms": unix_ms(),
    });
    match verdict {
        Verdict::Up { reply_subject } => {
            record["verdict"] = json!("up");
            record["detail"] = json!("bus.health.up");
            record["reply_subject"] = json!(reply_subject);
        }
        Verdict::Down {
            class,
            cause,
            message,
        } => {
            record["verdict"] = json!("down");
            record["detail"] = json!("bus.health.down");
            record["class"] = json!(class.as_str());
            record["cause"] = json!(cause);
            record["message"] = json!(message);
        }
    }
    record
}

/// Waits for bootstrap, then probes for the life of the process.
async fn run(
    monitor: Arc<Monitor>,
    mut ready: watch::Receiver<Option<Arc<Ready>>>,
    module_id: String,
    store: VerdictStore,
) {
    let ready = loop {
        if let Some(ready) = ready.borrow().clone() {
            break ready;
        }
        if ready.changed().await.is_err() {
            return;
        }
    };
    let persist = |verdict: &Verdict| -> Value {
        let record = verdict_record(verdict, monitor.incarnation());
        eprintln!("{record}");
        if let Err(error) = store.write(&record) {
            log_event(
                "ckbus.sentinel.verdict_unwritten",
                json!({ "path": store.path().display().to_string(), "error": error.to_string() }),
            );
        }
        record
    };
    let Ok(names) = grants::derive_account(&ready.account.acct) else {
        // Bootstrap derived the same names before it could finish, so this is
        // unreachable in practice; it is still reported rather than assumed.
        if let Some(verdict) = monitor.record(RoundOutcome::failed(
            Class::Unavailable,
            cause::LINK_ABSENT,
            format!("the account {} derives no names", ready.account.acct),
        )) {
            persist(&verdict);
        }
        return;
    };
    let Some(link) = ready.box_plane.sentinel_link() else {
        if let Some(verdict) = monitor.record(RoundOutcome::failed(
            Class::Unavailable,
            cause::LINK_ABSENT,
            "the box plane has no connection to probe on",
        )) {
            persist(&verdict);
        }
        return;
    };
    let timing = monitor.timing();
    loop {
        match NatsProbe::start(
            link.clone(),
            ready.box_plane.clone(),
            names.clone(),
            &module_id,
            monitor.incarnation(),
        )
        .await
        {
            Ok(probe) => {
                let probe = Arc::new(probe);
                let (published, mut to_publish) = tokio::sync::mpsc::unbounded_channel::<Value>();
                let publisher = probe.clone();
                tokio::spawn(async move {
                    while let Some(record) = to_publish.recv().await {
                        publisher.publish_verdict(&record, timing.timeout).await;
                    }
                });
                let changed = move |verdict: &Verdict| {
                    let _ = published.send(persist(verdict));
                };
                monitor.drive(probe.as_ref(), &changed).await;
                return;
            }
            Err(failure) => {
                if let Some(verdict) = monitor.record(failure) {
                    persist(&verdict);
                }
                tokio::time::sleep(timing.period).await;
            }
        }
    }
}

/// The one wiring call: starts the sentinel beside bootstrap and returns the health
/// answer that replaces bootstrap's own.
pub fn wire(
    bootstrap_health: Arc<dyn SentinelHealth>,
    ready: watch::Receiver<Option<Arc<Ready>>>,
    store_root: &std::path::Path,
    module_id: String,
    incarnation: String,
    timing: Timing,
) -> Arc<dyn SentinelHealth> {
    let store = VerdictStore::new(store_root);
    store.remove_stale_tmp();
    // The previous verdict is named for the operator and never answers for this
    // process.
    match store.read() {
        VerdictRead::Absent => {}
        VerdictRead::Present(previous) => log_event(
            "ckbus.sentinel.previous_verdict",
            json!({ "previous": previous, "used": false }),
        ),
        VerdictRead::Damaged { reason } => log_event(
            "ckbus.sentinel.previous_verdict",
            json!({
                "path": store.path().display().to_string(),
                "damaged": reason,
                "used": false,
            }),
        ),
    }
    let monitor = Arc::new(Monitor::new(incarnation, timing));
    tokio::spawn(run(monitor.clone(), ready.clone(), module_id, store));
    Arc::new(Answer::new(bootstrap_health, ready, monitor))
}

#[cfg(test)]
mod tests;
