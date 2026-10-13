//! Operator confirmation policy and its blocking provider boundary.
//!
//! The forwarding lock precedes the state lock. Providers and deliveries never
//! acquire the forwarding lock, and neither can hold up the policy timers.
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use subc_protocol::{
    error_codes,
    session::{OperatorConfirmReply, OperatorConfirmRequest},
    ErrorBody, Flags, FrameType, Principal, Priority,
};
use tokio::{
    sync::{oneshot, Notify},
    time::{timeout_at, Instant},
};

use crate::{
    caller_identity::CallerIdentity,
    forwarding::{ModuleEndpointId, ModuleRouteKey, RouteBinding},
    router::RouteCtx,
    ConnectionId, Frame,
};

/// Person declines of privileged client requests that start a cooldown when
/// they all fall within `DECLINE_WINDOW` of the newest one.
const DECLINE_LIMIT: usize = 3;
const DECLINE_WINDOW: Duration = Duration::from_secs(10 * 60);
/// Measured from the decline that started the cooldown. Refusals during the
/// cooldown are not declines by a person, so they never extend it.
const DECLINE_COOLDOWN: Duration = Duration::from_secs(10 * 60);

/// Monotonic time for the repeated-decline cooldown. Tests replace it so the
/// ten-minute window and cooldown can be crossed without waiting.
pub(crate) type CooldownClock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// Who repeated person declines of privileged requests are counted against.
/// The executable path keeps a reconnecting program under its cooldown; a
/// caller whose executable could not be resolved is counted per connection,
/// because nothing else the daemon owns ties its connections together.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DeclineKey {
    Executable(PathBuf),
    Connection(ConnectionId),
}

impl DeclineKey {
    pub(crate) fn for_caller(caller: &CallerIdentity, connection: ConnectionId) -> Self {
        match caller.executable() {
            Some(executable) => Self::Executable(executable.to_path_buf()),
            None => Self::Connection(connection),
        }
    }
}

impl std::fmt::Display for DeclineKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Executable(path) => write!(f, "{}", path.display()),
            Self::Connection(connection) => write!(
                f,
                "an unidentified local process on connection {}",
                connection.get()
            ),
        }
    }
}

/// A blocking provider publishes a withdraw handle as soon as one exists, then
/// returns only when its prompt has ended. Publishing is allowed to be delayed:
/// the daemon's timeout also covers setup and a delayed handle is still withdrawn.
pub trait OperatorProvider: Send + Sync + 'static {
    fn prompt(
        &self,
        text: &str,
        publish: Box<dyn FnOnce(Arc<dyn OperatorWithdraw>) + Send>,
    ) -> ProviderResult;
}

/// Called on a blocking thread, never while a daemon lock is held.
pub trait OperatorWithdraw: Send + Sync + 'static {
    fn withdraw(&self, reason: &str);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderResult {
    Approved,
    Declined,
    Unavailable,
    UnsupportedPlatform,
    NoPresence,
}

#[derive(Debug, Clone, Copy)]
pub struct OperatorLimits {
    pub queue_wait: Duration,
    pub prompt_timeout: Duration,
    pub stuck_grace: Duration,
}

impl Default for OperatorLimits {
    fn default() -> Self {
        Self {
            queue_wait: Duration::from_secs(150),
            prompt_timeout: Duration::from_secs(120),
            stuck_grace: Duration::from_secs(10),
        }
    }
}

impl OperatorLimits {
    fn delivery_budget(self) -> Duration {
        self.queue_wait + self.prompt_timeout + self.stuck_grace + Duration::from_secs(10)
    }
}

struct OsProvider;

#[cfg(any(target_os = "macos", windows))]
struct OsWithdraw(Arc<dyn subc_presence::Withdraw>);
#[cfg(any(target_os = "macos", windows))]
impl OperatorWithdraw for OsWithdraw {
    fn withdraw(&self, _: &str) {
        self.0.withdraw();
    }
}

impl OperatorProvider for OsProvider {
    fn prompt(
        &self,
        text: &str,
        publish: Box<dyn FnOnce(Arc<dyn OperatorWithdraw>) + Send>,
    ) -> ProviderResult {
        // On Linux the prompt comes from polkit, whose agents may show only the
        // fixed message of an installed action file, not this request's summary.
        // A person must see what they approve, so Linux refuses rather than prompts.
        #[cfg(target_os = "linux")]
        {
            let _ = (text, publish);
            ProviderResult::UnsupportedPlatform
        }
        #[cfg(any(target_os = "macos", windows))]
        {
            os_outcome(subc_presence::prompt(
                text,
                Box::new(move |handle| publish(Arc::new(OsWithdraw(handle)))),
            ))
        }
        #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
        {
            let _ = (text, publish);
            ProviderResult::Unavailable
        }
    }
}

/// Convert the operating-system prompt's result into the daemon's provider
/// result. Only an explicit approval may become `Approved`. The real prompt
/// needs a person, so tests can't reach this mapping through it; keeping it a
/// separate function lets a test check that no other result grants a write.
#[cfg(any(target_os = "macos", windows))]
fn os_outcome(outcome: subc_presence::Outcome) -> ProviderResult {
    match outcome {
        subc_presence::Outcome::Approved => ProviderResult::Approved,
        subc_presence::Outcome::Declined => ProviderResult::Declined,
        subc_presence::Outcome::ProviderError => ProviderResult::Unavailable,
        subc_presence::Outcome::NoPresence => ProviderResult::NoPresence,
        subc_presence::Outcome::UnsupportedPlatform => ProviderResult::UnsupportedPlatform,
    }
}

#[cfg(all(test, any(target_os = "macos", windows)))]
mod os_outcome_tests {
    use super::*;

    #[test]
    fn only_an_os_approval_becomes_approved() {
        use subc_presence::Outcome as Os;
        assert_eq!(os_outcome(Os::Approved), ProviderResult::Approved);
        assert_eq!(os_outcome(Os::Declined), ProviderResult::Declined);
        assert_eq!(os_outcome(Os::ProviderError), ProviderResult::Unavailable);
        assert_eq!(os_outcome(Os::NoPresence), ProviderResult::NoPresence);
        assert_eq!(
            os_outcome(Os::UnsupportedPlatform),
            ProviderResult::UnsupportedPlatform
        );
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Outcome {
    pub code: &'static str,
    pub reason: &'static str,
}
impl Outcome {
    pub(crate) fn unavailable(reason: &'static str) -> Self {
        Self {
            code: error_codes::OPERATOR_PRESENCE_UNAVAILABLE,
            reason,
        }
    }
    fn declined(reason: &'static str) -> Self {
        Self {
            code: error_codes::OPERATOR_DECLINED,
            reason,
        }
    }
    pub(crate) fn refusal(code: &'static str) -> Self {
        Self { code, reason: "" }
    }
    fn from_provider(result: ProviderResult) -> Self {
        match result {
            ProviderResult::Approved => Self {
                code: "confirmed",
                reason: "",
            },
            ProviderResult::Declined => Self::declined("person"),
            ProviderResult::Unavailable => Self::unavailable("provider_error"),
            ProviderResult::UnsupportedPlatform => Self::unavailable("unsupported_platform"),
            ProviderResult::NoPresence => Self::unavailable("no_presence"),
        }
    }
    pub(crate) fn frame(self, request: &Frame) -> Frame {
        let (ty, body) = if self.code == "confirmed" {
            (
                FrameType::Response,
                serde_json::to_vec(&OperatorConfirmReply::confirmed()).expect("serializable reply"),
            )
        } else {
            let mut error = ErrorBody::new(self.code, "operator confirmation refused");
            if !self.reason.is_empty() {
                error = error.with_detail(serde_json::json!({"reason": self.reason}));
            }
            (
                FrameType::Error,
                serde_json::to_vec(&error).expect("serializable error"),
            )
        };
        Frame::build_with_version(
            request.header.ver,
            ty,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            request.header.corr,
            body,
        )
        .expect("bounded control reply")
    }
}

pub(crate) fn audit(
    module_id: &str,
    summary: &str,
    principal: &str,
    outcome: Outcome,
    wait: Duration,
    prompt: Duration,
    prompt_shown: bool,
) {
    tracing::info!(target: "subc_daemon::operator_confirm", module_id, summary = ?summary,
        principal, outcome = outcome.code, reason = outcome.reason,
        wait_ms = wait.as_millis() as u64, prompt_ms = prompt.as_millis() as u64,
        prompt_shown, "operator_confirm_audit");
}

fn principal_label(principal: &Principal) -> String {
    match principal {
        Principal::Direct => "direct".into(),
        Principal::Reserved { module_id } => format!("reserved:{module_id}"),
        Principal::Unverified => String::new(),
    }
}

fn valid_summary(summary: &str) -> bool {
    let count = summary.chars().count();
    (1..=200).contains(&count)
        && !summary.starts_with(char::is_whitespace)
        && !summary.ends_with(char::is_whitespace)
        && !summary.chars().any(|c| {
            c.is_control()
                || matches!(c,
            '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2028}' | '\u{2029}' |
            '\u{2060}'..='\u{2069}' | '\u{feff}')
        })
}

enum RequestKind {
    Module(ModuleRouteKey),
    Privileged {
        answer: Mutex<Option<oneshot::Sender<Outcome>>>,
        key: DeclineKey,
    },
}

struct Request {
    id: u64,
    module_id: String,
    summary: String,
    principal: String,
    text: String,
    kind: RequestKind,
    ctx: RouteCtx,
    frame: Frame,
    arrived: Instant,
    delivery_deadline: Instant,
}
struct Active {
    request: Arc<Request>,
    started: Option<Instant>,
    handle: Option<Arc<dyn OperatorWithdraw>>,
    committed: bool,
    withdrawn: Option<(Instant, &'static str)>,
    withdraw_started: bool,
}
struct State {
    provider: Arc<dyn OperatorProvider>,
    limits: OperatorLimits,
    queue: VecDeque<Arc<Request>>,
    modules: HashMap<String, u64>,
    backoff: HashMap<(String, String), Instant>,
    cooldown_clock: CooldownClock,
    /// Recent person declines of privileged requests, oldest first.
    declines: HashMap<DeclineKey, VecDeque<Instant>>,
    /// When each privileged caller's cooldown ends.
    cooldowns: HashMap<DeclineKey, Instant>,
    prompt: Option<Active>,
    stuck: bool,
    running: bool,
    next_id: u64,
    #[cfg(test)]
    deliveries: usize,
}

pub(crate) struct OperatorConfirms {
    state: Mutex<State>,
    wake: Notify,
}

struct WithdrawOnDrop {
    confirms: Arc<OperatorConfirms>,
    id: u64,
}

impl Drop for WithdrawOnDrop {
    fn drop(&mut self) {
        self.confirms
            .withdraw_matching(|request| request.id == self.id, "caller_cancelled");
    }
}
impl std::fmt::Debug for OperatorConfirms {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperatorConfirms").finish_non_exhaustive()
    }
}
impl Default for OperatorConfirms {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                provider: Arc::new(OsProvider),
                limits: OperatorLimits::default(),
                queue: VecDeque::new(),
                modules: HashMap::new(),
                backoff: HashMap::new(),
                cooldown_clock: Arc::new(Instant::now),
                declines: HashMap::new(),
                cooldowns: HashMap::new(),
                prompt: None,
                stuck: false,
                running: false,
                next_id: 0,
                #[cfg(test)]
                deliveries: 0,
            }),
            wake: Notify::new(),
        }
    }
}
impl OperatorConfirms {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
    pub(crate) fn configure(&self, provider: Arc<dyn OperatorProvider>, limits: OperatorLimits) {
        let mut state = self.lock();
        assert!(
            !state.running && state.modules.is_empty(),
            "configure before serving"
        );
        state.provider = provider;
        state.limits = limits;
    }
    pub(crate) fn configure_bootstrap(&self, config: &crate::bootstrap::BootstrapConfig) {
        let provider: Arc<dyn OperatorProvider> = Arc::new(OsProvider);
        #[cfg(feature = "test-support")]
        let provider =
            test_provider::select(provider, config, std::env::current_exe().ok().as_deref());
        self.configure(
            provider,
            OperatorLimits {
                queue_wait: config.operator_queue_wait,
                prompt_timeout: config.operator_timeout,
                stuck_grace: config.operator_stuck_grace,
            },
        );
    }

    /// Called while the forwarding read lock still protects this binding. All
    /// registration/nonce lookups must have finished before entering this method.
    pub(crate) fn admit(
        self: &Arc<Self>,
        ctx: &RouteCtx,
        frame: Frame,
        module_id: String,
        nonce_proven: bool,
        request: OperatorConfirmRequest,
        binding: Option<&RouteBinding>,
    ) -> Vec<Frame> {
        let principal = binding
            .map(|b| principal_label(&b.principal))
            .unwrap_or_default();
        let mut state = self.lock();
        let now = Instant::now();
        state.backoff.retain(|_, until| *until > now);
        let refusal = if !nonce_proven
            || binding.is_none_or(|b| matches!(b.principal, Principal::Unverified))
        {
            Some(Outcome::refusal(
                error_codes::OPERATOR_REQUEST_NOT_PERMITTED,
            ))
        } else if !valid_summary(&request.summary) {
            Some(Outcome::refusal(error_codes::OPERATOR_SUMMARY_INVALID))
        } else if state
            .backoff
            .contains_key(&(module_id.clone(), principal.clone()))
        {
            Some(Outcome::declined("backoff"))
        } else if state.stuck {
            Some(Outcome::unavailable("provider_stuck"))
        } else if state.modules.contains_key(&module_id) {
            Some(Outcome::unavailable("module_limit"))
        } else if state.queue.len() >= 4 {
            Some(Outcome::unavailable("queue_full"))
        } else {
            None
        };
        if let Some(outcome) = refusal {
            audit(
                &module_id,
                &request.summary,
                &principal,
                outcome,
                Duration::ZERO,
                Duration::ZERO,
                false,
            );
            return vec![outcome.frame(&frame)];
        }
        let binding = binding.expect("permission checked");
        let opener = match &binding.principal {
            Principal::Direct => "a local program",
            Principal::Reserved { module_id } => module_id,
            Principal::Unverified => unreachable!(),
        };
        state.next_id += 1;
        let pending = Arc::new(Request {
            id: state.next_id,
            text: format!(
                "approve a request from {module_id}: {} (requested by {opener})",
                request.summary
            ),
            module_id,
            summary: request.summary,
            principal,
            kind: RequestKind::Module(ModuleRouteKey {
                endpoint: binding.module_endpoint,
                channel: binding.module_channel,
            }),
            ctx: ctx.clone(),
            frame,
            arrived: now,
            delivery_deadline: now + state.limits.delivery_budget(),
        });
        state.modules.insert(pending.module_id.clone(), pending.id);
        self.enqueue(&mut state, pending);
        Vec::new()
    }

    /// Client-control approval shares the module confirmation queue and timers,
    /// but returns a decision to dispatch instead of sending an operator reply.
    /// A caller in its repeated-decline cooldown is refused before anything else
    /// is checked, without a prompt.
    pub(crate) async fn approve_privileged(
        self: &Arc<Self>,
        ctx: &RouteCtx,
        frame: &Frame,
        operation: &str,
        text: String,
        caller: String,
        key: DeclineKey,
    ) -> Outcome {
        let (answer, receive) = oneshot::channel();
        let id = {
            let mut state = self.lock();
            let now = (state.cooldown_clock)();
            state.cooldowns.retain(|_, until| *until > now);
            state.declines.retain(|_, declines| {
                declines.retain(|at| now.saturating_duration_since(*at) < DECLINE_WINDOW);
                !declines.is_empty()
            });
            let refusal = if state.cooldowns.contains_key(&key) {
                Some(Outcome::declined("backoff"))
            } else if state.stuck {
                Some(Outcome::unavailable("provider_stuck"))
            } else if state.queue.len() >= 4 {
                Some(Outcome::unavailable("queue_full"))
            } else {
                None
            };
            if let Some(outcome) = refusal {
                audit(
                    operation,
                    &text,
                    &caller,
                    outcome,
                    Duration::ZERO,
                    Duration::ZERO,
                    false,
                );
                return outcome;
            }
            state.next_id += 1;
            let id = state.next_id;
            let now = Instant::now();
            let pending = Arc::new(Request {
                id,
                module_id: operation.into(),
                summary: text.clone(),
                principal: caller,
                text,
                kind: RequestKind::Privileged {
                    answer: Mutex::new(Some(answer)),
                    key,
                },
                ctx: ctx.clone(),
                frame: frame.clone(),
                arrived: now,
                delivery_deadline: now + state.limits.delivery_budget(),
            });
            self.enqueue(&mut state, pending);
            id
        };
        let _withdraw = WithdrawOnDrop {
            confirms: Arc::clone(self),
            id,
        };
        receive
            .await
            .unwrap_or_else(|_| Outcome::unavailable("provider_error"))
    }

    fn enqueue(self: &Arc<Self>, state: &mut State, pending: Arc<Request>) {
        if state.prompt.is_none() && state.queue.is_empty() {
            state.prompt = Some(Active {
                request: pending,
                started: None,
                handle: None,
                committed: false,
                withdrawn: None,
                withdraw_started: false,
            });
        } else {
            state.queue.push_back(pending);
        }
        if !state.running {
            state.running = true;
            tokio::spawn(Arc::clone(self).run());
        }
        self.wake.notify_one();
    }

    fn commit(
        self: &Arc<Self>,
        state: &mut State,
        request: Arc<Request>,
        outcome: Outcome,
        started: Option<Instant>,
        shown: bool,
    ) {
        let now = Instant::now();
        let wait = started.unwrap_or(now).duration_since(request.arrived);
        let prompt = started.map(|at| now.duration_since(at)).unwrap_or_default();
        if outcome.reason == "person" && matches!(request.kind, RequestKind::Module(_)) {
            state.backoff.insert(
                (request.module_id.clone(), request.principal.clone()),
                now + Duration::from_secs(30),
            );
        }
        audit(
            &request.module_id,
            &request.summary,
            &request.principal,
            outcome,
            wait,
            prompt,
            shown,
        );
        if let RequestKind::Privileged { answer, key } = &request.kind {
            if let Some(answer) = answer.lock().unwrap_or_else(|p| p.into_inner()).take() {
                let _ = answer.send(outcome);
            }
            if outcome.reason == "person" {
                self.count_person_decline(state, key);
            }
            return;
        }
        let this = Arc::clone(self);
        #[cfg(test)]
        {
            state.deliveries += 1;
        }
        tokio::spawn(async move {
            let answer = outcome.frame(&request.frame);
            match timeout_at(request.delivery_deadline, request.ctx.egress.send(answer)).await {
                Ok(Ok(())) => {}
                result => {
                    tracing::warn!(target: "subc_daemon::operator_confirm", module_id = request.module_id, ?result, "operator confirm answer dropped")
                }
            }
            let mut state = this.lock();
            #[cfg(test)]
            {
                state.deliveries -= 1;
            }
            if state.modules.get(&request.module_id) == Some(&request.id) {
                state.modules.remove(&request.module_id);
            }
        });
    }

    /// Only a person's decline at the prompt reaches here: withdrawals, timeouts
    /// and unavailability commit other reasons. The prompt that recorded the
    /// limit-reaching decline has already ended, so starting the cooldown only
    /// refuses this caller's requests still waiting in the queue.
    fn count_person_decline(self: &Arc<Self>, state: &mut State, key: &DeclineKey) {
        let now = (state.cooldown_clock)();
        let declines = state.declines.entry(key.clone()).or_default();
        declines.retain(|at| now.saturating_duration_since(*at) < DECLINE_WINDOW);
        declines.push_back(now);
        if declines.len() < DECLINE_LIMIT {
            return;
        }
        // Counting restarts after the cooldown instead of carrying old declines.
        state.declines.remove(key);
        state.cooldowns.insert(key.clone(), now + DECLINE_COOLDOWN);
        tracing::warn!(target: "subc_daemon::operator_confirm", caller = %key,
            cooldown_secs = DECLINE_COOLDOWN.as_secs(),
            "privileged requests refused without a prompt after repeated operator declines");
        let mut index = 0;
        while index < state.queue.len() {
            if matches!(&state.queue[index].kind,
                RequestKind::Privileged { key: queued, .. } if queued == key)
            {
                let request = state.queue.remove(index).expect("queue index");
                self.commit(state, request, Outcome::declined("backoff"), None, false);
            } else {
                index += 1;
            }
        }
    }

    fn withdraw_matching(
        self: &Arc<Self>,
        predicate: impl Fn(&Request) -> bool,
        reason: &'static str,
    ) -> bool {
        let mut state = self.lock();
        let mut matched = false;
        let mut index = 0;
        while index < state.queue.len() {
            if predicate(&state.queue[index]) {
                let request = state.queue.remove(index).expect("queue index");
                self.commit(&mut state, request, Outcome::declined(reason), None, false);
                matched = true;
            } else {
                index += 1;
            }
        }
        if let Some(active) = state
            .prompt
            .as_mut()
            .filter(|a| !a.committed && predicate(&a.request))
        {
            active.committed = true;
            active.withdrawn = Some((Instant::now(), reason));
            let (request, started, shown) = (
                Arc::clone(&active.request),
                active.started,
                active.handle.is_some(),
            );
            self.commit(
                &mut state,
                request,
                Outcome::declined(reason),
                started,
                shown,
            );
            matched = true;
        }
        self.wake.notify_one();
        matched
    }
    pub(crate) fn route_closed(self: &Arc<Self>, key: ModuleRouteKey) {
        self.withdraw_matching(
            |r| matches!(r.kind, RequestKind::Module(k) if k == key),
            "route_closed",
        );
    }
    pub(crate) fn module_closed(self: &Arc<Self>, endpoint: ModuleEndpointId) {
        self.withdraw_matching(
            |r| matches!(r.kind, RequestKind::Module(k) if k.endpoint == endpoint),
            "module_closed",
        );
    }
    pub(crate) fn connection_closed(self: &Arc<Self>, connection: crate::ConnectionId) {
        self.withdraw_matching(
            |r| {
                matches!(r.kind, RequestKind::Privileged { .. })
                    && r.ctx.connection_id == connection
            },
            "caller_cancelled",
        );
    }
    pub(crate) fn cancel(self: &Arc<Self>, connection: crate::ConnectionId, corr: u64) -> bool {
        self.withdraw_matching(
            |r| r.ctx.connection_id == connection && r.frame.header.corr == corr,
            "caller_cancelled",
        )
    }

    fn publish(self: &Arc<Self>, id: u64, handle: Arc<dyn OperatorWithdraw>) {
        let mut state = self.lock();
        if let Some(active) = state.prompt.as_mut().filter(|a| a.request.id == id) {
            active.handle = Some(Arc::clone(&handle));
            if let Some((_, reason)) = active.withdrawn {
                active.withdraw_started = true;
                tokio::task::spawn_blocking(move || handle.withdraw(reason));
            }
        }
        self.wake.notify_one();
    }
    fn returned(self: &Arc<Self>, id: u64, result: ProviderResult) {
        let mut state = self.lock();
        let Some(active) = state.prompt.take() else {
            return;
        };
        assert_eq!(active.request.id, id);
        // A fast late return can beat the prompt task's wake-up. The decision
        // still requires a withdraw, even though the result is discarded.
        if !active.withdraw_started {
            if let (Some(handle), Some((_, reason))) = (active.handle.as_ref(), active.withdrawn) {
                let handle = Arc::clone(handle);
                tokio::task::spawn_blocking(move || handle.withdraw(reason));
            }
        }
        if !active.committed {
            self.commit(
                &mut state,
                active.request,
                Outcome::from_provider(result),
                active.started,
                active.handle.is_some(),
            );
        } else {
            tracing::debug!(target: "subc_daemon::operator_confirm", ?result, "late operator provider result discarded");
        }
        state.stuck = false;
        self.wake.notify_one();
    }

    async fn run(self: Arc<Self>) {
        loop {
            let deadline = {
                let mut state = self.lock();
                let now = Instant::now();
                let mut index = 0;
                while index < state.queue.len() {
                    if state.queue[index].arrived + state.limits.queue_wait <= now {
                        let request = state.queue.remove(index).expect("queue index");
                        self.commit(
                            &mut state,
                            request,
                            Outcome::unavailable("queue_wait"),
                            None,
                            false,
                        );
                    } else {
                        index += 1;
                    }
                }
                let timeout = state.limits.prompt_timeout;
                if let Some(active) = state
                    .prompt
                    .as_mut()
                    .filter(|a| !a.committed && a.started.is_some_and(|at| at + timeout <= now))
                {
                    active.committed = true;
                    active.withdrawn = Some((now, "timeout"));
                    let (request, started, shown) = (
                        Arc::clone(&active.request),
                        active.started,
                        active.handle.is_some(),
                    );
                    self.commit(
                        &mut state,
                        request,
                        Outcome::unavailable("timeout"),
                        started,
                        shown,
                    );
                }
                let grace = state.limits.stuck_grace;
                if !state.stuck
                    && state.prompt.as_ref().is_some_and(|a| {
                        a.started.is_some() && a.withdrawn.is_some_and(|(at, _)| at + grace <= now)
                    })
                {
                    state.stuck = true;
                    tracing::error!(target: "subc_daemon::operator_confirm", "operator provider stuck after withdraw");
                    while let Some(request) = state.queue.pop_front() {
                        self.commit(
                            &mut state,
                            request,
                            Outcome::unavailable("provider_stuck"),
                            None,
                            false,
                        );
                    }
                }
                if let Some(active) = state
                    .prompt
                    .as_mut()
                    .filter(|a| !a.withdraw_started && a.withdrawn.is_some() && a.handle.is_some())
                {
                    active.withdraw_started = true;
                    let handle = Arc::clone(active.handle.as_ref().expect("handle checked"));
                    let reason = active.withdrawn.expect("withdraw checked").1;
                    tokio::task::spawn_blocking(move || handle.withdraw(reason));
                }
                // A reserved prompt cancelled before setup needs no provider call.
                if state
                    .prompt
                    .as_ref()
                    .is_some_and(|a| a.started.is_none() && a.committed)
                {
                    state.prompt = None;
                }
                if state.prompt.is_none() {
                    if let Some(request) = state.queue.pop_front() {
                        state.prompt = Some(Active {
                            request,
                            started: None,
                            handle: None,
                            committed: false,
                            withdrawn: None,
                            withdraw_started: false,
                        });
                    }
                }
                if let Some(active) = state.prompt.as_mut().filter(|a| a.started.is_none()) {
                    active.started = Some(now);
                    let request = Arc::clone(&active.request);
                    let provider = Arc::clone(&state.provider);
                    let this = Arc::clone(&self);
                    tokio::spawn(async move {
                        let publish_to = Arc::clone(&this);
                        let id = request.id;
                        let result = tokio::task::spawn_blocking(move || {
                            provider.prompt(
                                &request.text,
                                Box::new(move |handle| publish_to.publish(id, handle)),
                            )
                        })
                        .await
                        .unwrap_or(ProviderResult::Unavailable);
                        this.returned(id, result);
                    });
                }
                if state.prompt.is_none() && state.queue.is_empty() {
                    state.running = false;
                    return;
                }
                let mut deadline = state
                    .queue
                    .iter()
                    .map(|r| r.arrived + state.limits.queue_wait)
                    .min();
                if let Some(active) = &state.prompt {
                    let next = if !active.committed {
                        active.started.map(|at| at + timeout)
                    } else if !state.stuck {
                        active.withdrawn.map(|(at, _)| at + grace)
                    } else {
                        None
                    };
                    if let Some(next) = next {
                        deadline = Some(deadline.map_or(next, |d| d.min(next)));
                    }
                }
                deadline
            };
            if let Some(at) = deadline {
                tokio::select! { _ = self.wake.notified() => {}, _ = tokio::time::sleep_until(at) => {} }
            } else {
                self.wake.notified().await;
            }
        }
    }
}

#[cfg(feature = "test-support")]
mod test_provider;
#[cfg(test)]
mod tests;
