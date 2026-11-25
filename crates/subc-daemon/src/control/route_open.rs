use super::*;

/// How long subc waits for a module to ack a relayed route.bind before returning
/// `module_timeout`. The ack waits on the module's own configure, which for AFT
/// includes a synchronous bounded project walk (up to ~20k files) plus gitignore
/// and DB-open work — on a cold page cache or a large repo that legitimately
/// exceeds a couple of seconds. The default is generous because rejecting a VALID
/// bind is far worse than waiting on a slow one; a consumer that wants a tighter
/// bound retries the bind itself (the sanctioned warm-bind-retry pattern).
pub const DEFAULT_ROUTE_BIND_RELAY_TIMEOUT: Duration = Duration::from_secs(12);

/// How many CONSECUTIVE full-budget relay timeouts against one target module
/// open that module's bind-relay breaker.
///
/// Three, so that the breaker is NOT REACHABLE INSIDE ONE CLIENT CALL. Both
/// SDKs default to a 30s request deadline and the relay budget defaults to 12s,
/// so three consecutive full-budget timeouts take ~36s to observe: every client
/// whose open contributed to opening the breaker had already given up on its
/// own. That is what makes opening the breaker unable to turn a call that would
/// have succeeded into a refusal — it can only make an already-failing module
/// fail faster.
///
/// Two would be reachable inside one default deadline. One would convict a
/// module on a single cold-cache bind, which is exactly the valid-but-slow case
/// `DEFAULT_ROUTE_BIND_RELAY_TIMEOUT`'s own doc comment exists to protect.
pub const DEFAULT_ROUTE_BIND_BREAKER_THRESHOLD: u32 = 3;

/// How long a module's bind-relay breaker stays open before exactly one
/// `route.open` is let through as a probe.
///
/// Bounded BELOW by the relay budget: a cooldown at or under the 12s budget
/// re-pays a full-budget stall almost continuously, and the breaker stops being
/// a saving worth its own state. Bounded ABOVE by the SDKs' 30s default request
/// deadline: a client that starts retrying after the module recovers has to get
/// a probe opportunity inside its own deadline, or the breaker converts a
/// recovered module into a failed call — the failure it exists to prevent,
/// pointed the other way.
///
/// 20s sits between those with room on both sides, and it caps what a wedged
/// module can cost at one full-budget wait per 20s ACROSS THE WHOLE DAEMON
/// rather than one per `route.open` per connection. The stall that motivated
/// this, with its measurements, is written up in
/// `docs/designs/route-open-head-of-line.md`: 268 opens against one module each
/// waited the whole budget out.
pub const DEFAULT_ROUTE_BIND_BREAKER_COOLDOWN: Duration = Duration::from_secs(20);

/// Refusal labels passed to `observe_route_open_refusal` that mean the target
/// module is not serving right now, and so open or extend an outage in the
/// route outage tracker. Every one of them is only reachable after the target
/// was found in the registry, which is what keeps an arbitrary client-chosen
/// id from ever creating tracker state.
///
/// Deliberately absent: `not_registered` and `removed` (the id may be
/// anything a client sent, and a removed module is gone on purpose),
/// `protocol_none` (such a module never serves routes, so nothing is out),
/// `role_not_provided`, `op_not_allowed`, `bad_consumer_identity`, the
/// capability and admission-facts refusals (they refuse the caller, not a
/// module outage), and `relay_reservation_failed` (its code ranges over
/// capacity limits as well as a vanished connection). Capacity, breaker,
/// relay-timeout and module-rejection refusals do not pass through that
/// function at all; the breaker logs its own transitions.
///
/// The two not-serving refusals that bypass that function record themselves
/// at their own sites: `supervised_not_registered` and `declared_not_ready`.
/// `required_capability_unprovided` is not tracked: the module itself is up,
/// and the outage belongs to the missing provider.
const ROUTE_OPEN_NOT_SERVING_REASONS: &[&str] = &[
    "reloading",
    "supervisor_not_live",
    "registration_not_active",
    "no_forwarding_connection",
    "relay_send_failed",
];

pub(super) struct RouteOpenRequest {
    pub(super) target: RouteTarget,
    pub(super) identity: BindIdentity,
    pub(super) consumer_identity: Option<ConsumerIdentity>,
    pub(super) consumer_capabilities: Option<Vec<String>>,
    pub(super) role_versions: Option<BTreeMap<String, String>>,
    pub(super) busy_refusals: Option<bool>,
    pub(super) admission_facts: Option<serde_json::Value>,
    pub(super) scope: Option<ScopeSelector>,
    pub(super) accepts_close_reasons: Option<Vec<String>>,
}

#[derive(Clone, Copy)]
enum RouteOpenPhase {
    Admission,
    ProjectRoot,
    EgressReservation,
    BindSend,
    BindAckWait,
}

/// Store elapsed time for each route-open phase, including attempts that end
/// in an early refusal or backend error. Admission also includes rechecks and
/// commit bookkeeping performed between the other timed phases.
pub(super) struct RouteOpenTimings {
    pub(super) module_id: String,
    pub(super) principal: String,
    pub(super) admission: Duration,
    pub(super) project_root: Duration,
    pub(super) egress_reservation: Duration,
    pub(super) bind_send: Duration,
    pub(super) bind_ack_wait: Duration,
    phase: Option<RouteOpenPhase>,
    phase_started_at: Instant,
}

impl RouteOpenTimings {
    pub(super) fn new() -> Self {
        Self {
            module_id: String::new(),
            // Refusals before attestation must not claim a verified principal.
            principal: "unresolved".to_string(),
            admission: Duration::ZERO,
            project_root: Duration::ZERO,
            egress_reservation: Duration::ZERO,
            bind_send: Duration::ZERO,
            bind_ack_wait: Duration::ZERO,
            phase: Some(RouteOpenPhase::Admission),
            phase_started_at: Instant::now(),
        }
    }

    pub(super) fn finish(&mut self) {
        let elapsed = self.phase_started_at.elapsed();
        match self.phase.take() {
            Some(RouteOpenPhase::Admission) => self.admission += elapsed,
            Some(RouteOpenPhase::ProjectRoot) => self.project_root += elapsed,
            Some(RouteOpenPhase::EgressReservation) => self.egress_reservation += elapsed,
            Some(RouteOpenPhase::BindSend) => self.bind_send += elapsed,
            Some(RouteOpenPhase::BindAckWait) => self.bind_ack_wait += elapsed,
            None => {}
        }
    }

    fn enter(&mut self, phase: RouteOpenPhase) {
        self.finish();
        self.phase = Some(phase);
        self.phase_started_at = Instant::now();
    }
}

struct RouteBindReservationGuard {
    forwarding: Arc<ForwardingTable>,
    endpoint: ModuleEndpointId,
    relay_corr: u64,
    armed: bool,
}

impl RouteBindReservationGuard {
    fn new(forwarding: Arc<ForwardingTable>, endpoint: ModuleEndpointId, relay_corr: u64) -> Self {
        Self {
            forwarding,
            endpoint,
            relay_corr,
            armed: true,
        }
    }

    fn release_and_disarm(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(Some(target)) = self.forwarding.abort_pending_relay(
            self.endpoint,
            self.relay_corr,
            RouteBindRelayOutcome::ModuleGone("route.open handler canceled".to_string()),
        ) {
            send_goodbye_target_best_effort(
                &self.forwarding.counters(),
                &target,
                "canceled route.bind",
            );
        }
        self.armed = false;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RouteBindReservationGuard {
    fn drop(&mut self) {
        self.release_and_disarm();
    }
}

/// Per-target-module circuit breaker around the `route.bind` relay.
///
/// The connection reader is serial per connection, so a module whose `on_bind`
/// sits on the ack blocks every LATER frame on the connections that call it,
/// including calls to unrelated modules. This does not make any module's bind
/// fast; it stops the daemon paying the full budget again and again for a
/// condition it has already observed.
///
/// State is keyed by TARGET MODULE and shared by every connection: a wedged
/// module wedges everyone, so what one connection learned should protect the
/// rest.
///
/// THE MAP IS EMPTY WHILE THE FLEET IS HEALTHY. An entry appears only when a
/// relay to that module has actually timed out, and is removed again when a
/// relay is accepted or the module reconnects, so it cannot grow with traffic
/// or with modules that behave.
///
/// # Why a `std` mutex here is not the head-of-line defect again
///
/// Acquisition never awaits. The critical section is a hash lookup plus a few
/// integer updates, with no I/O and no `.await` inside it, so a reader task
/// cannot be descheduled behind it the way it can behind
/// `tokio::sync::Mutex::lock().await` or a semaphore permit. It is the same
/// primitive, held for the same kind of work, as the refusal counter this very
/// path already increments.
///
/// It is also NOT on the data-plane splice path: only `route.open` and module
/// registration touch it, so bound-route frames gain no state check and no
/// contention.
#[derive(Debug, Clone, Default)]
pub(crate) struct RouteBindBreakers {
    modules: Arc<Mutex<HashMap<String, ModuleBreakerState>>>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RouteBindConcurrency {
    modules: Arc<Mutex<HashMap<String, usize>>>,
}

pub(super) struct RouteBindConcurrencyGuard {
    concurrency: RouteBindConcurrency,
    module_id: String,
}

impl RouteBindConcurrency {
    /// Admit without waiting. Waiting here would move the bind stall from the
    /// module reply to a semaphore and restore reader head-of-line blocking.
    pub(super) fn try_admit(
        &self,
        module_id: &str,
        limit: usize,
    ) -> Result<RouteBindConcurrencyGuard, usize> {
        let mut modules = self
            .modules
            .lock()
            .expect("route.bind concurrency mutex poisoned");
        let in_flight = modules.entry(module_id.to_string()).or_default();
        if *in_flight >= limit {
            return Err(*in_flight);
        }
        *in_flight += 1;
        Ok(RouteBindConcurrencyGuard {
            concurrency: self.clone(),
            module_id: module_id.to_string(),
        })
    }
}

impl Drop for RouteBindConcurrencyGuard {
    fn drop(&mut self) {
        let mut modules = self
            .concurrency
            .modules
            .lock()
            .expect("route.bind concurrency mutex poisoned");
        let remove = {
            let in_flight = modules
                .get_mut(&self.module_id)
                .expect("admitted route.bind has a concurrency entry");
            *in_flight -= 1;
            *in_flight == 0
        };
        if remove {
            modules.remove(&self.module_id);
        }
    }
}

#[derive(Debug, Default)]
struct ModuleBreakerState {
    /// Relay timeouts observed with no accepted relay in between.
    consecutive_timeouts: u32,
    /// `Some` while the breaker is open: the instant the cooldown expires and
    /// the next arrival may probe. `None` means closed.
    cooldown_until: Option<Instant>,
    /// A half-open probe has been admitted and has not settled yet. This is
    /// what makes the probe EXACTLY ONE: the flag is set under the same lock
    /// that read the cooldown, so concurrent opens arriving at the moment the
    /// cooldown expires cannot all decide that they are the probe.
    probe_in_flight: Option<Arc<()>>,
}

/// What the breaker decided for one `route.open`, before any relay work.
pub(super) enum RouteBindAdmission<'a> {
    Admitted {
        guard: RouteBindBreakerGuard<'a>,
        /// This open is the single half-open probe, so the transition is worth
        /// one log line.
        probe: bool,
    },
    Refused {
        consecutive_timeouts: u32,
        /// What is left of the cooldown. Zero when the refusal is because the
        /// one probe is already in flight rather than because the cooldown has
        /// not elapsed.
        retry_in: Duration,
        probe_in_flight: bool,
    },
}

/// An outstanding admission, which must be told how its relay settled.
///
/// `Drop` settles it as inconclusive, so an early return between admission and
/// the relay -- or the whole handler being cancelled when the client
/// disconnects -- releases a half-open probe slot instead of leaving the
/// breaker wedged half-open with no further probes.
pub(super) struct RouteBindBreakerGuard<'a> {
    breakers: RouteBindBreakers,
    module_id: &'a str,
    probe_token: Option<Arc<()>>,
    settled: bool,
}

impl RouteBindBreakerGuard<'_> {
    /// The module answered within the budget and took the bind. THE ONLY
    /// OUTCOME THAT CLEARS THE COUNT. Returns true when this closed an open
    /// breaker, which is a transition worth logging.
    fn record_accepted(&mut self) -> bool {
        self.settled = true;
        self.breakers.record_accepted(self.module_id)
    }

    /// The relay burned the whole budget with no answer. THE ONLY ARM THAT
    /// COUNTS TOWARD OPENING.
    pub(super) fn record_timeout(
        &mut self,
        threshold: u32,
        cooldown: Duration,
    ) -> Option<BreakerOpened> {
        self.settled = true;
        self.breakers.record_timeout(
            self.module_id,
            self.probe_token.as_ref(),
            threshold,
            cooldown,
        )
    }

    /// Everything else: the module REJECTED the bind, its connection went away
    /// mid-relay, or the waiter was cancelled.
    ///
    /// None of these is evidence that a module is slow, and each already has
    /// its own refusal with its own code. A module that rejects a bind in
    /// microseconds is healthy and must never be convicted for it; a module
    /// that died has said nothing about the module that replaces it. So these
    /// neither increment nor reset the count -- they only release a probe slot.
    pub(super) fn record_inconclusive(&mut self) {
        self.settled = true;
        self.breakers
            .record_inconclusive(self.module_id, self.probe_token.as_ref());
    }
}

impl Drop for RouteBindBreakerGuard<'_> {
    fn drop(&mut self) {
        if !self.settled {
            self.breakers
                .record_inconclusive(self.module_id, self.probe_token.as_ref());
        }
    }
}

/// The breaker moved to open, reported so the caller can log it outside the
/// lock. Opening is rare and load-bearing; the refusals that follow are
/// frequent and are counted rather than logged.
pub(super) struct BreakerOpened {
    consecutive_timeouts: u32,
    /// True when a failed probe re-opened an already-open breaker, which reads
    /// very differently in a log from a first opening.
    pub(super) reopened_after_probe: bool,
}

impl RouteBindBreakers {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, ModuleBreakerState>> {
        self.modules
            .lock()
            .expect("route.bind breaker mutex poisoned")
    }

    /// Decide whether this `route.open` may attempt its relay. Takes the map
    /// lock and nothing else, and never awaits.
    pub(super) fn admit<'a>(&self, module_id: &'a str) -> RouteBindAdmission<'a> {
        let admitted = |probe_token: Option<Arc<()>>| RouteBindAdmission::Admitted {
            probe: probe_token.is_some(),
            guard: RouteBindBreakerGuard {
                breakers: self.clone(),
                module_id,
                probe_token,
                settled: false,
            },
        };

        let mut modules = self.lock();
        let Some(state) = modules.get_mut(module_id) else {
            return admitted(None);
        };
        let Some(cooldown_until) = state.cooldown_until else {
            return admitted(None);
        };
        if state.probe_in_flight.is_some() {
            return RouteBindAdmission::Refused {
                consecutive_timeouts: state.consecutive_timeouts,
                retry_in: Duration::ZERO,
                probe_in_flight: true,
            };
        }
        let now = Instant::now();
        if now < cooldown_until {
            return RouteBindAdmission::Refused {
                consecutive_timeouts: state.consecutive_timeouts,
                retry_in: cooldown_until - now,
                probe_in_flight: false,
            };
        }
        let token = Arc::new(());
        state.probe_in_flight = Some(Arc::clone(&token));
        admitted(Some(token))
    }

    fn record_accepted(&self, module_id: &str) -> bool {
        self.lock()
            .remove(module_id)
            .is_some_and(|state| state.cooldown_until.is_some())
    }

    pub(super) fn record_timeout(
        &self,
        module_id: &str,
        probe_token: Option<&Arc<()>>,
        threshold: u32,
        cooldown: Duration,
    ) -> Option<BreakerOpened> {
        let mut modules = self.lock();
        let state = modules.entry(module_id.to_string()).or_default();
        let was_open = state.cooldown_until.is_some();
        let was_probe = Self::owns_probe(state, probe_token);
        if was_probe {
            state.probe_in_flight = None;
        }
        state.consecutive_timeouts = state.consecutive_timeouts.saturating_add(1);
        if state.consecutive_timeouts < threshold {
            return None;
        }
        state.cooldown_until = Some(Instant::now() + cooldown);
        Some(BreakerOpened {
            consecutive_timeouts: state.consecutive_timeouts,
            reopened_after_probe: was_open && was_probe,
        })
    }

    fn owns_probe(state: &ModuleBreakerState, token: Option<&Arc<()>>) -> bool {
        // A relay can finish after the breaker was reset or after it opened
        // again and started a new probe. Only the guard whose token matches the
        // active probe may release it, so a late relay never frees a newer probe.
        state
            .probe_in_flight
            .as_ref()
            .zip(token)
            .is_some_and(|(active, token)| Arc::ptr_eq(active, token))
    }

    fn record_inconclusive(&self, module_id: &str, probe_token: Option<&Arc<()>>) {
        if let Some(state) = self.lock().get_mut(module_id) {
            if Self::owns_probe(state, probe_token) {
                state.probe_in_flight = None;
            }
        }
    }

    /// Discard what was learned about a module, because the process it was
    /// learned about is gone. Returns the discarded count when it was non-zero.
    ///
    /// A BREAKER IS A CACHED VERDICT ABOUT A PROCESS, NOT ABOUT A NAME. A
    /// `module_id` is a configuration identity that outlives any particular
    /// child; what the breaker observed was the process behind the module
    /// connection of the moment. When a new connection registers under that id
    /// the verdict's subject no longer exists, so the verdict is stale by
    /// construction rather than merely likely to be wrong. Keeping it would
    /// apply a dead process's record to a live one, which is the same defect
    /// class this breaker exists to stop the daemon committing.
    ///
    /// A half-open probe in flight is discarded with the rest: it was a
    /// question about the old process.
    pub(crate) fn reset_for_new_module_connection(&self, module_id: &str) -> Option<u32> {
        self.lock()
            .remove(module_id)
            .map(|state| state.consecutive_timeouts)
            .filter(|discarded| *discarded > 0)
    }

    /// Open breakers, for the `server.describe` counters object. `None` when
    /// none is open, so the key stays absent rather than present-and-empty.
    ///
    /// This is the operator's answer to "is this module refusing instantly or
    /// is it fine?", which look identical from a client that retries and then
    /// succeeds.
    pub(super) fn open_snapshot(&self) -> Option<serde_json::Value> {
        let now = Instant::now();
        let modules = self.lock();
        let open = modules
            .iter()
            .filter_map(|(module_id, state)| {
                let cooldown_until = state.cooldown_until?;
                Some((
                    module_id.clone(),
                    serde_json::json!({
                        "consecutive_timeouts": state.consecutive_timeouts,
                        "cooldown_remaining_ms":
                            cooldown_until.saturating_duration_since(now).as_millis() as u64,
                        "probe_in_flight": state.probe_in_flight.is_some(),
                    }),
                ))
            })
            .collect::<serde_json::Map<String, serde_json::Value>>();
        (!open.is_empty()).then_some(serde_json::Value::Object(open))
    }
}

impl ControlHandler {
    /// Override the route.bind relay timeout. Used by tests that assert the
    /// timeout path so they don't block on the production-safe default.
    pub fn with_route_bind_relay_timeout(mut self, timeout: Duration) -> Self {
        self.route_bind_relay_timeout = timeout;
        self
    }

    /// Install per-module route.bind relay budget overrides. A module id
    /// listed here wins over the daemon-wide default set via
    /// `with_route_bind_relay_timeout`. Values are pre-resolved at parse time
    /// from `subc.jsonc` (per-module > daemon-wide > absent), so callers pass
    /// the same `Duration` the bind path will use.
    pub fn with_route_bind_relay_timeouts(
        mut self,
        timeouts: impl IntoIterator<Item = (String, Duration)>,
    ) -> Self {
        self.route_bind_relay_timeouts = timeouts.into_iter().collect();
        self
    }

    /// Resolve the route.bind relay budget for a specific target module id.
    /// Per-module overrides win; the daemon-wide value (set via
    /// `with_route_bind_relay_timeout` or the built-in default) is the
    /// fallback. Exposed so config-aware callers (bootstrap, tests) can audit
    /// the same resolution `handle_route_open` will use.
    pub fn route_bind_relay_timeout_for(&self, module_id: &str) -> Duration {
        self.route_bind_relay_timeouts
            .get(module_id)
            .copied()
            .unwrap_or(self.route_bind_relay_timeout)
    }

    /// Override the per-module bind-relay breaker policy.
    ///
    /// Used by tests, which cannot spend three production budgets opening a
    /// breaker or twenty seconds waiting for its cooldown. The production
    /// values are `DEFAULT_ROUTE_BIND_BREAKER_THRESHOLD` and
    /// `DEFAULT_ROUTE_BIND_BREAKER_COOLDOWN`, whose doc comments carry the
    /// reasoning for the numbers.
    pub fn with_route_bind_breaker(mut self, threshold: u32, cooldown: Duration) -> Self {
        self.route_bind_breaker_threshold = threshold.max(1);
        self.route_bind_breaker_cooldown = cooldown;
        self
    }
}

impl ControlHandler {
    /// `route.open` waits on `hold` twice after queueing a route.bind: once to
    /// announce it is parked, once to be released. Build the barrier for two
    /// parties, the test being the other one.
    #[cfg(test)]
    pub(crate) fn with_route_bind_relay_sent_hold(
        mut self,
        hold: Arc<tokio::sync::Barrier>,
    ) -> Self {
        self.route_bind_relay_sent_hold = Some(hold);
        self
    }
}

impl ControlHandler {
    pub(crate) fn route_open_target(&self, frame: &Frame) -> Option<String> {
        if frame.header.channel != 0 || frame.header.ty != FrameType::Request {
            return None;
        }
        let Ok(ClientControlRequest::RouteOpen { target, .. }) =
            parse_client_control_request(&frame.body)
        else {
            return None;
        };
        Some(target_module_id(&target).to_string())
    }

    pub(crate) fn route_open_capacity_refusal(
        &self,
        ctx: &RouteCtx,
        frame: &Frame,
        target_module_id: &str,
        in_flight: usize,
        limit: usize,
    ) -> Result<Frame, RouterError> {
        // Sent before the request reaches `handle_route_open`, so the caller
        // fields come from the raw frame here.
        let caller = self.route_open_caller_from_frame(ctx, frame);
        self.route_open_admission_refusal_frame(
            ctx,
            &caller,
            frame,
            target_module_id,
            "open_admission_full",
            (in_flight, limit),
            format!(
                "connection already has {in_flight} route.open binds in flight (limit {limit}); retry after one settles"
            ),
        )
    }

    pub(super) fn route_open_target_capacity_refusal(
        &self,
        ctx: &RouteCtx,
        caller: &RouteOpenCaller,
        frame: &Frame,
        target_module_id: &str,
        in_flight: usize,
    ) -> Result<Frame, RouterError> {
        self.route_open_admission_refusal_frame(
            ctx,
            caller,
            frame,
            target_module_id,
            "target_binds_full",
            (in_flight, MAX_PENDING_ROUTE_BINDS_PER_TARGET),
            format!(
                "module_id '{target_module_id}' already has {in_flight} route.bind relays in flight; retry after one settles"
            ),
        )
    }

    /// Admission pressure clears as existing binds settle, so its refusal must
    /// remain in the deployed SDKs' closed retryable set: `unknown_module`,
    /// `module_reloading`, `module_warming`, `target_unavailable`, or
    /// `module_timeout`. `target_unavailable` is honest for an attempt that
    /// cannot currently reach its target; `module_timeout` would falsely claim
    /// that a wait expired. A new, cleaner code would be terminal to deployed
    /// clients, so it requires a client-tolerance rollout before daemon emission.
    #[allow(clippy::too_many_arguments)]
    fn route_open_admission_refusal_frame(
        &self,
        ctx: &RouteCtx,
        caller: &RouteOpenCaller,
        frame: &Frame,
        target_module_id: &str,
        reason: &'static str,
        (in_flight, limit): (usize, usize),
        message: impl Into<String>,
    ) -> Result<Frame, RouterError> {
        let code = error_codes::TARGET_UNAVAILABLE;
        self.counters.increment_route_open_refused(code);
        info!(
            target: "control",
            code,
            reason,
            module_id = ?target_module_id,
            connection_id = ctx.connection_id.get(),
            principal = %caller.principal,
            scope = %caller.scope,
            harness = %caller.harness,
            in_flight,
            limit,
            "route.open refused"
        );
        control_error_frame(frame, code, message.into())
    }
}

impl ControlHandler {
    /// Best-effort GOODBYE to a module for a route channel subc reserved but then
    /// abandoned (route.bind relay timed out, its waiter was cancelled, or subc's
    /// own commit failed after the module had already accepted). Without this, a
    /// module that accepts late keeps a binding subc has torn down, so a later
    /// frame on that module channel could misdeliver if the channel is reused.
    ///
    /// Never closes the shared module connection on failure: a dropped notification
    /// only wastes a bounded amount of warm module-side state, which the module's
    /// own idle reaper reclaims. Only call this once the route.bind relay was
    /// actually enqueued to the module — if the relay send itself failed, the
    /// module never created a binding and there is nothing to tear down.
    fn send_abandoned_route_bind_goodbye(
        &self,
        module_sink: &crate::FrameSink,
        negotiated_ver: u8,
        module_channel: u16,
        module_epoch: u32,
    ) {
        let frame = match Frame::build_with_version(
            negotiated_ver,
            FrameType::Goodbye,
            control_flags(),
            module_channel,
            module_epoch,
            0,
            Vec::new(),
        ) {
            Ok(frame) => frame,
            Err(err) => {
                warn!(
                    route_channel = module_channel,
                    error = %err,
                    "failed to build GOODBYE for abandoned route.bind"
                );
                return;
            }
        };
        crate::forwarding::send_module_route_goodbye(
            &self.counters,
            module_sink,
            frame,
            None,
            "abandoned route.bind",
        );
    }
}

impl ControlHandler {
    /// The caller fields for a refusal that only has the raw request frame,
    /// such as the connection-level admission refusal sent before the request
    /// reaches [`Self::handle_route_open`].
    fn route_open_caller_from_frame(&self, ctx: &RouteCtx, frame: &Frame) -> RouteOpenCaller {
        let Ok(ClientControlRequest::RouteOpen {
            identity,
            consumer_identity,
            scope,
            ..
        }) = parse_client_control_request(&frame.body)
        else {
            return RouteOpenCaller::unknown();
        };
        let principal = self
            .attest_route_open_consumer(ctx.connection_id, consumer_identity.as_ref())
            .map(|attested| attested.principal);
        RouteOpenCaller::new(principal.as_ref(), scope.as_ref(), &identity)
    }

    pub(super) fn route_open_principal(
        &self,
        connection_id: ConnectionId,
        frame: &Frame,
        consumer_identity: Option<ConsumerIdentity>,
    ) -> Result<Result<Principal, Frame>, RouterError> {
        let attested = self
            .attest_route_open_consumer(connection_id, consumer_identity.as_ref())
            .map(|attested| attested.principal);
        self.route_open_principal_attested(frame, consumer_identity, attested)
    }

    /// [`Self::route_open_principal`] with `attested` already taken by
    /// [`Self::attest_route_open_consumer`], so one open resolves its connection
    /// claim or launch nonce once.
    fn route_open_principal_attested(
        &self,
        frame: &Frame,
        consumer_identity: Option<ConsumerIdentity>,
        attested: Option<Principal>,
    ) -> Result<Result<Principal, Frame>, RouterError> {
        if let Some(principal) = attested {
            return Ok(Ok(principal));
        }
        Ok(Err(control_error_frame(
            frame,
            "bad_consumer_identity",
            consumer_identity.map_or_else(
                || "connection claim is no longer authorized for routes".to_string(),
                |identity| format!(
                    "consumer_identity for module_id '{}' conflicts with its connection claim or did not match a supervised launch nonce",
                    identity.module_id
                ),
            ),
        )?))
    }

    /// Ordinary `route.open` refusals go through here; admission and breaker
    /// refusals log separately with their capacity or breaker state. The daemon can
    /// attest which code it sent: without the event, a client's "the daemon
    /// refused me" and the daemon's own view could only be reconciled by
    /// argument. Malformed input (`invalid_project_root`) does not come here;
    /// rejecting a request that was never a valid open is not a refusal of one.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn route_open_refusal_frame(
        &self,
        ctx: &RouteCtx,
        caller: &RouteOpenCaller,
        frame: &Frame,
        module_id: &str,
        reason: &'static str,
        code: &'static str,
        message: impl Into<String>,
    ) -> Result<Frame, RouterError> {
        self.observe_route_open_refusal(ctx, caller, module_id, reason, code);
        let detail = if matches!(
            reason,
            "registration_not_active"
                | "supervisor_not_live"
                | "no_forwarding_connection"
                | "reloading"
        ) {
            // A planned restart, reload or swap is already replacing the
            // process; it does not need a slot in the crash-restart budget.
            let expected_back = (reason == "reloading").then_some(true);
            self.route_open_recovery_detail(module_id, frame.header.corr, None, expected_back)?
        } else {
            None
        };
        control_error_body_frame(
            frame,
            ErrorBody {
                code: code.to_string(),
                message: message.into(),
                detail,
            },
        )
    }

    fn route_open_recovery_detail(
        &self,
        module_id: &str,
        corr: u64,
        detail: Option<serde_json::Value>,
        expected_back: Option<bool>,
    ) -> Result<Option<serde_json::Value>, RouterError> {
        let Some((status, _)) = self.supervisor_status(module_id, corr)? else {
            return Ok(detail);
        };
        let mut detail = detail.unwrap_or_else(|| serde_json::json!({}));
        detail["state"] = serde_json::json!(status.state.to_string());
        if let Some(expected_back) = expected_back {
            detail["expected_back"] = serde_json::json!(expected_back);
        } else if let Some(module) = self.supervisor.get(module_id) {
            match module.will_recover_after_connection_loss() {
                Ok(expected_back) => detail["expected_back"] = serde_json::json!(expected_back),
                Err(err) => warn!(
                    %module_id,
                    error = %err,
                    "failed to read route.open recovery verdict; omitting expected_back"
                ),
            }
        }
        Ok(Some(detail))
    }

    /// Answer a waiting `route.open` whose relayed bind was refused.
    ///
    /// `daemon_authored` is provenance, not a code match: the daemon
    /// synthesized the refusal itself (a drain, a late reply, a scope that
    /// ended mid-bind) or the module refused the bind with its own body. A
    /// daemon refusal carries the daemon-origin bit; a module's own
    /// rejection is relayed with the bit clear, so a module that refuses
    /// with `module_reloading` still cannot mint a daemon refusal.
    #[allow(clippy::too_many_arguments)]
    fn route_bind_rejection(
        &self,
        ctx: &RouteCtx,
        caller: &RouteOpenCaller,
        frame: &Frame,
        target_module_id: &str,
        body: ErrorBody,
        daemon_authored: bool,
    ) -> Result<Vec<Frame>, RouterError> {
        // The daemon's own commit re-check refused the bind because the
        // scope ended or changed after admission. The module accepted;
        // counting it as a module rejection would blame the module. This
        // branch only runs for daemon-synthesized bodies: a module that
        // refuses with a scope code is answered as a module rejection below.
        if daemon_authored {
            let scope_code = match body.code.as_str() {
                error_codes::SCOPE_CHANGED => Some(error_codes::SCOPE_CHANGED),
                error_codes::SCOPE_ENDED => Some(error_codes::SCOPE_ENDED),
                _ => None,
            };
            if let Some(code) = scope_code {
                self.observe_route_open_refusal(
                    ctx,
                    caller,
                    target_module_id,
                    "scope_changed_before_commit",
                    code,
                );
                return Ok(vec![control_error_body_frame(frame, body)?]);
            }
        }
        self.counters
            .increment_route_open_refused("module_rejected");
        info!(
            target: "control",
            code = "module_rejected",
            module_code = ?body.code,
            module_id = ?target_module_id,
            connection_id = ctx.connection_id.get(),
            principal = %caller.principal,
            scope = %caller.scope,
            harness = %caller.harness,
            "route.open refused"
        );
        let refusal = if daemon_authored {
            control_error_body_frame(frame, body)?
        } else {
            control_relayed_error_body_frame(frame, body)?
        };
        Ok(vec![refusal])
    }

    /// Refuse a `route.open` because the target module's bind-relay breaker is
    /// open, without attempting the relay.
    ///
    /// The wire code is `module_timeout`, which is the truth (the module has
    /// not been answering binds) and which both SDKs already classify as
    /// retryable with capped backoff. Reusing it is what keeps this change out
    /// of both SDKs; the daemon-side distinction lives in the counter key
    /// instead.
    ///
    /// DELIBERATELY NOT LOGGED PER OCCURRENCE, unlike every other refusal.
    /// While a breaker is open this fires on every open to that module, and the
    /// stall written up in `docs/designs/route-open-head-of-line.md` already
    /// produced 261 lines about a single module inside 3000 lines of daemon
    /// log. The rare transitions are logged at warn/info instead and the volume
    /// is carried by the counter, so the evidence survives without the flood.
    /// The debug line keeps a per-refusal record reachable for whoever turns
    /// the level up.
    #[allow(clippy::too_many_arguments)]
    fn route_open_breaker_refusal_frame(
        &self,
        ctx: &RouteCtx,
        caller: &RouteOpenCaller,
        frame: &Frame,
        module_id: &str,
        consecutive_timeouts: u32,
        retry_in: Duration,
        probe_in_flight: bool,
    ) -> Result<Frame, RouterError> {
        self.counters
            .increment_route_open_refused(crate::observability::ROUTE_OPEN_REFUSED_BREAKER_OPEN);
        debug!(
            target: "control",
            code = "module_timeout",
            module_id = ?module_id,
            connection_id = ctx.connection_id.get(),
            principal = %caller.principal,
            scope = %caller.scope,
            harness = %caller.harness,
            consecutive_timeouts,
            retry_in_ms = retry_in.as_millis() as u64,
            probe_in_flight,
            "route.open refused by open bind-relay breaker"
        );
        // Say what a caller can act on. An open bind-relay breaker means the
        // module timed out accepting several new routes in a row. The module
        // is still running and its established routes keep working; only new
        // route.open requests are refused until the cooldown ends and one
        // test route (the probe) gets through. A message that only counts
        // failed relays reads as "the module is down" to a worker that sees it.
        let detail = if probe_in_flight {
            "one test route is already being tried; retry once it settles".to_string()
        } else {
            format!("retrying new routes in {}s", retry_in.as_secs().max(1))
        };
        control_error_frame(
            frame,
            "module_timeout",
            format!(
                "module '{module_id}' is slow to accept new routes ({consecutive_timeouts} \
                 timed out in a row); {detail}; its established routes are unaffected"
            ),
        )
    }

    /// `reason` names the check that refused, because one wire code has
    /// several senders: after a module registers, `target_unavailable` can
    /// come from a missing role, an inactive registration, a supervisor that
    /// has not marked the process live, a missing forwarding connection, or a
    /// failed relay, and a log that records only the code cannot say which of
    /// them fired. It is a static, daemon-chosen label per branch, so it is
    /// safe to print plainly and stays a closed set.
    fn observe_route_open_refusal(
        &self,
        ctx: &RouteCtx,
        caller: &RouteOpenCaller,
        module_id: &str,
        reason: &'static str,
        code: &'static str,
    ) {
        self.counters.increment_route_open_refused(code);
        self.route_refusal_log
            .record(module_id, code, reason, caller, ctx.connection_id.get());
        if ROUTE_OPEN_NOT_SERVING_REASONS.contains(&reason) {
            self.route_outages.record_not_serving(module_id, reason);
        }
    }

    /// Record an ACCEPTED route.open.
    ///
    /// Refusals have been logged and counted since the attestation work; accepts
    /// were invisible, so the daemon knew every principal it stamped and wrote
    /// none of them down. The party that attests the identity was the only party
    /// not recording it, which left a credential vault unable to name the sender
    /// of a call that reached it (claustrum #43) and left the launch-nonce
    /// concurrency question unanswerable from the outside.
    ///
    /// FIELD NAMES MATCH `route.open refused` DELIBERATELY, so one grep over
    /// `code`/`module_id`/`connection_id` returns both directions of the same
    /// decision rather than two shapes a reader has to join by hand.
    ///
    /// `module_id` IS RENDERED BARE HERE AND DEBUG-ESCAPED ON THE REFUSAL PATH,
    /// and the difference carries information rather than being an
    /// inconsistency. This line is only reachable after a successful bind to a
    /// REGISTERED module, so the value has already passed HELLO validation
    /// including the path-hazard refusal and cannot contain control bytes. A
    /// refused id may be arbitrary attacker-chosen bytes and must stay escaped.
    /// So A QUOTED `module_id` IN THE LOG MEANS THE VALUE WAS NEVER VALIDATED.
    ///
    /// Bare is also what every other daemon line already emits (`module
    /// registered`, `configured module supervised`). Shipping `?module_id` here
    /// made this instrument the only one in the file whose ids did not answer
    /// `grep module_id=broca` -- 3 hits against 342 for the escaped form, in a
    /// line whose whole purpose is being grepped beside its sibling.
    ///
    /// THIS RENDERING IS UNFENCED AND THE REASON IS WORTH KNOWING: the in-crate
    /// `EventCapture` test layer implements only `record_debug`, so `Visit`
    /// forwards every field type through it and a bare `&str` and a `?`-escaped
    /// one are recorded identically. A test written against that harness passes
    /// either way -- I wrote one, measured it, and deleted it rather than ship a
    /// green assertion that cannot fail. The same limit applies to the escaping
    /// assertion in `route_open_supervised_absence_emits_refusal_fields_and_counts_code`:
    /// it reads as a guard on the Debug escaping and cannot detect its removal.
    /// Fencing either needs the real formatter, not the capture layer.
    ///
    /// `peer_addr` is NOT here and cannot be: `SO_PEERCRED`/`LOCAL_PEERPID` are
    /// unix-socket options and subc is loopback TCP, so there is no peer identity
    /// to record. The ephemeral port would decay within minutes and answer only a
    /// live question. The identity question is instead answered by counting
    /// distinct live connections presenting one module's `consumer_identity` --
    /// "is anyone else holding this secret" rather than "is this the right
    /// process".
    fn observe_route_open_accept(
        &self,
        ctx: &RouteCtx,
        module_id: &str,
        principal: &str,
        scope: &str,
    ) {
        self.route_outages.record_accepted(module_id);
        self.counters.increment_route_open_accepted(principal);
        debug!(
            target: "control",
            principal,
            module_id,
            connection_id = ctx.connection_id.get(),
            scope = %scope,
            "route.open accepted"
        );
    }

    fn supervised_absent_route_open_refusal_frame(
        &self,
        ctx: &RouteCtx,
        caller: &RouteOpenCaller,
        frame: &Frame,
        module_id: &str,
        code: &'static str,
        status: &crate::supervise::ModuleStatus,
    ) -> Result<Frame, RouterError> {
        self.counters.increment_route_open_refused(code);
        info!(
            target: "control",
            code,
            reason = "supervised_not_registered",
            module_id = ?module_id,
            connection_id = ctx.connection_id.get(),
            principal = %caller.principal,
            scope = %caller.scope,
            harness = %caller.harness,
            state = %status.state,
            enabled = status.enabled,
            live = status.live,
            "route.open refused"
        );
        // A supervised module whose process has not registered is not
        // serving, whatever the reason; the supervisor knows this id, so it is
        // safe to track.
        self.route_outages
            .record_not_serving(module_id, "supervised_not_registered");
        let message = if let Some(pid) = status.waiting_for_previous_pid {
            status.waiting_for_previous_detail.clone().unwrap_or_else(|| {
                format!("waiting: previous instance (pid {pid}) is still running but could not be verified")
            })
        } else {
            format!(
                "module_id '{module_id}' is supervised but not available (state={}, enabled={}, live={})",
                status.state, status.enabled, status.live
            )
        };
        // `module_warming` here means the process is starting, running or
        // being restarted and has not registered yet. Nothing has to be
        // restarted for it to serve, so the crash budget does not decide it.
        let expected_back = (code == error_codes::MODULE_WARMING).then_some(true);
        control_error_body_frame(
            frame,
            ErrorBody {
                code: code.to_string(),
                message,
                detail: self.route_open_recovery_detail(
                    module_id,
                    frame.header.corr,
                    None,
                    expected_back,
                )?,
            },
        )
    }

    pub(super) async fn handle_route_open(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        request: RouteOpenRequest,
        timings: &mut RouteOpenTimings,
    ) -> Result<Vec<Frame>, RouterError> {
        let RouteOpenRequest {
            target,
            mut identity,
            consumer_identity,
            consumer_capabilities,
            role_versions,
            busy_refusals,
            admission_facts,
            scope,
            accepts_close_reasons,
        } = request;
        let target_module_id = target_module_id(&target).to_string();
        timings.module_id.clone_from(&target_module_id);
        if self
            .registry
            .get_module_by_connection(ctx.connection_id)
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?
            .is_some()
        {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_request",
                "module connections cannot open client routes",
            )?]);
        }
        debug!(
            connection_id = ctx.connection_id.get(),
            corr = frame.header.corr,
            module_id = %target_module_id,
            "handling route.open"
        );

        // The caller's attested principal, requested scope and harness, for
        // every `route.open refused` line below. The connection's claim
        // binding or launch nonce is checked here, once, and the answer is
        // reused where the principal is enforced further down, so the order
        // of refusals is unchanged.
        let attested =
            self.attest_route_open_consumer(ctx.connection_id, consumer_identity.as_ref());
        let mut caller = RouteOpenCaller::new(
            attested.as_ref().map(|attested| &attested.principal),
            scope.as_ref(),
            &identity,
        );

        // A malformed declaration is refused first, before anything about the
        // target is looked up: the same body would be refused against any
        // module, so the caller learns nothing by retrying or waiting. An empty
        // map declares nothing and travels as no field at all, so a provider
        // only ever sees a missing field or a non-empty one.
        let role_versions = role_versions.filter(|role_versions| !role_versions.is_empty());
        if let Some(Err(error)) = role_versions.as_ref().map(validate_role_versions) {
            self.observe_route_open_refusal(
                ctx,
                &caller,
                &target_module_id,
                "invalid_role_versions",
                error_codes::INVALID_REQUEST,
            );
            return Ok(vec![control_error_body_frame(
                &frame,
                ErrorBody {
                    code: error_codes::INVALID_REQUEST.to_string(),
                    message: error.to_string(),
                    detail: Some(serde_json::json!({ "field": ROLE_VERSIONS_FIELD })),
                },
            )?]);
        }

        if let Some((status, _)) = self.supervisor_status(&target_module_id, frame.header.corr)? {
            if status.waiting_for_previous_pid.is_some() {
                return Ok(vec![self.supervised_absent_route_open_refusal_frame(
                    ctx,
                    &caller,
                    &frame,
                    &target_module_id,
                    "module_warming",
                    &status,
                )?]);
            }
        }

        // WHY THESE REPLIES DISCRIMINATE FREELY, since the usual rule is the
        // opposite. Below, a caller learns whether a module is unregistered,
        // supervised-but-down (with state/enabled/live), or registered without the
        // requested role. Elsewhere that is an enumeration leak: a probe learning
        // the shape of a fleet it cannot otherwise see.
        //
        // It is not one here, and the reason is the ACCESS MODEL rather than
        // anything about these errors. Reaching route.open requires the
        // pre-envelope HMAC handshake, whose key lives in a 0600 user-owned
        // connection file, so any caller who completes it already runs as this
        // user -- and can read subc.jsonc for the module list and `ck module
        // status` for live state. The reply discloses nothing the caller cannot
        // read more easily from disk, while the precision is load-bearing:
        // `unknown_module` is retryable and a missing role is not.
        //
        // IF THE HANDSHAKE EVER ADMITS A PRINCIPAL THAT IS NOT THIS USER -- a
        // remote transport, a sandboxed caller, a shared-host mode -- THAT
        // PREMISE DIES AND THESE THREE REPLIES MUST COLLAPSE INTO ONE.
        let admission_registry = self
            .registry
            .route_admission_snapshot()
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?;
        let Some(registration) =
            crate::scopes::admission::registered_target(&admission_registry, &target_module_id)
        else {
            if let Some((status, warming)) =
                self.supervisor_status(&target_module_id, frame.header.corr)?
            {
                // BEFORE the two availability codes below, because for a module
                // that speaks no subc wire both of them are false comfort: they
                // say "not right now" and are retried, and this module will
                // never register no matter how long the caller waits. The
                // absence here is the declaration being honoured, not a module
                // that is late.
                if status.protocol == ModuleProtocol::None {
                    return Ok(vec![self.route_open_refusal_frame(
                        ctx,
                        &caller,
                        &frame,
                        &target_module_id,
                        "protocol_none",
                        error_codes::MODULE_NO_PROTOCOL,
                        format!(
                            "module_id '{target_module_id}' is declared protocol: none; \
                             it speaks no subc wire and serves no routes"
                        ),
                    )?]);
                }
                let code = if warming {
                    "module_warming"
                } else {
                    "target_unavailable"
                };
                return Ok(vec![self.supervised_absent_route_open_refusal_frame(
                    ctx,
                    &caller,
                    &frame,
                    &target_module_id,
                    code,
                    &status,
                )?]);
            }
            if let Some(removed_ago_ms) =
                self.supervisor.removal_tombstone_age_ms(&target_module_id)
            {
                return Ok(vec![self.route_open_refusal_frame(
                    ctx,
                    &caller,
                    &frame,
                    &target_module_id,
                    "removed",
                    error_codes::MODULE_REMOVED,
                    format!("module_id '{target_module_id}' was removed {removed_ago_ms} ms ago"),
                )?]);
            }
            return Ok(vec![self.route_open_refusal_frame(
                ctx,
                &caller,
                &frame,
                &target_module_id,
                "not_registered",
                error_codes::UNKNOWN_MODULE,
                format!("module_id '{target_module_id}' is not registered"),
            )?]);
        };

        // Best-effort only: registry readiness and forwarding reservation use
        // different locks, so a module can flip readiness between this read and
        // the relay. Modules must still tolerate an `on_bind` while not ready.
        if crate::scopes::admission::target_registration(&admission_registry, &target_module_id)
            .is_err()
        {
            self.counters
                .increment_route_open_refused(ROUTE_OPEN_REFUSED_DECLARED_NOT_READY);
            info!(
                target: "control",
                code = error_codes::MODULE_WARMING,
                module_id = ?target_module_id,
                connection_id = ctx.connection_id.get(),
                principal = %caller.principal,
                scope = %caller.scope,
                harness = %caller.harness,
                reason = "declared_not_ready",
                "route.open refused"
            );
            // The module is registered but says it cannot take work, which is
            // an outage from the caller's side even though its process is up.
            self.route_outages
                .record_not_serving(&target_module_id, "declared_not_ready");
            // The registered process can declare itself ready without a
            // restart, so an exhausted crash budget does not prevent serving.
            return Ok(vec![control_error_body_frame(
                &frame,
                ErrorBody {
                    code: error_codes::MODULE_WARMING.to_string(),
                    message: format!(
                        "module_id '{target_module_id}' is registered and has declared itself not ready; retry"
                    ),
                    detail: self.route_open_recovery_detail(
                        &target_module_id,
                        frame.header.corr,
                        Some(serde_json::json!({ "reason": "declared_not_ready" })),
                        Some(true),
                    )?,
                },
            )?]);
        }

        // Effective readiness, second half: a module that declares a capability
        // `need: required` is not routable while that capability has no
        // registered provider. It is enforced HERE, as a retryable routing
        // refusal, and deliberately not as spawn ordering or a boot block. The
        // module is still started and registered and can make its own calls;
        // spawn ordering is a promise that cannot be kept once a provider
        // crashes at runtime, and refusing to boot would stop the whole
        // machine, including the tools needed to fix its configuration.
        //
        // "Provided" is the evaluator's verdict, which counts a provider as
        // soon as it has REGISTERED, not once it is ready. Two modules that
        // require each other's capabilities are therefore both routable once
        // both register; counting readiness instead would deadlock them.
        //
        // Only new opens are refused. Routes already bound when a provider
        // goes away stay bound: nothing here tears them down, and the module
        // answers them as it can. Like the readiness read above this is
        // best-effort against a provider registering or leaving concurrently.
        if let Some(capability) = self.first_unprovided_required_capability_for(
            &registration.module_id,
            registration.capabilities.as_ref(),
        ) {
            self.counters
                .increment_route_open_refused(ROUTE_OPEN_REFUSED_REQUIRED_CAPABILITY_UNPROVIDED);
            info!(
                target: "control",
                code = error_codes::MODULE_WARMING,
                module_id = ?target_module_id,
                connection_id = ctx.connection_id.get(),
                principal = %caller.principal,
                scope = %caller.scope,
                harness = %caller.harness,
                reason = NotReadyReason::REQUIRED_CAPABILITY_UNPROVIDED,
                capability = %capability,
                "route.open refused"
            );
            return Ok(vec![control_error_body_frame(
                &frame,
                ErrorBody {
                    code: error_codes::MODULE_WARMING.to_string(),
                    message: format!(
                        "module_id '{target_module_id}' requires capability '{capability}', \
                         which no registered module provides; retry"
                    ),
                    detail: Some(serde_json::json!({
                        "reason": NotReadyReason::REQUIRED_CAPABILITY_UNPROVIDED,
                        "capability": capability,
                    })),
                },
            )?]);
        }

        if !target_has_required_role(&target, &registration.roles) {
            return Ok(vec![self.route_open_refusal_frame(
                ctx,
                &caller,
                &frame,
                &target_module_id,
                "role_not_provided",
                "target_unavailable",
                format!("module_id '{target_module_id}' does not provide the requested target"),
            )?]);
        }

        if registration.state != ChannelState::Active {
            return Ok(vec![self.route_open_refusal_frame(
                ctx,
                &caller,
                &frame,
                &target_module_id,
                "registration_not_active",
                "target_unavailable",
                format!("module_id '{target_module_id}' is not active"),
            )?]);
        }

        if self
            .forwarding
            .module_is_draining(&target_module_id)
            .map_err(RouterError::Forwarding)?
        {
            return Ok(vec![self.route_open_refusal_frame(
                ctx,
                &caller,
                &frame,
                &target_module_id,
                "reloading",
                "module_reloading",
                format!("module_id '{target_module_id}' is reloading"),
            )?]);
        }

        if let Some(process_liveness) = self.process_liveness.as_ref().filter(|process_liveness| {
            process_liveness.process_live(&target_module_id) == Some(false)
        }) {
            // A module the supervisor is restarting or reloading can still hold
            // a registration: the old process before its connection closes, or
            // a new one that registered while the supervisor was draining. The
            // forwarding table does not see that as draining, but the consumer
            // should still be told to retry soon, exactly as for the drain
            // above, rather than that the target is unavailable.
            if process_liveness.process_replacing(&target_module_id) {
                return Ok(vec![self.route_open_refusal_frame(
                    ctx,
                    &caller,
                    &frame,
                    &target_module_id,
                    "reloading",
                    "module_reloading",
                    format!("module_id '{target_module_id}' is reloading"),
                )?]);
            }
            return Ok(vec![self.route_open_refusal_frame(
                ctx,
                &caller,
                &frame,
                &target_module_id,
                "supervisor_not_live",
                "target_unavailable",
                format!("module_id '{target_module_id}' is not live"),
            )?]);
        }

        if !self
            .forwarding
            .has_live_module_connection(&target_module_id)
            .map_err(RouterError::Forwarding)?
        {
            return Ok(vec![self.route_open_refusal_frame(
                ctx,
                &caller,
                &frame,
                &target_module_id,
                "no_forwarding_connection",
                "target_unavailable",
                format!("module_id '{target_module_id}' has no live forwarding connection"),
            )?]);
        }

        if let Some(error) =
            self.guard_module_control_op(&frame, &target_module_id, "route.bind")?
        {
            self.observe_route_open_refusal(
                ctx,
                &caller,
                &target_module_id,
                "op_not_allowed",
                "op_not_allowed",
            );
            return Ok(vec![error]);
        }

        let principal = match (&attested, consumer_identity.is_some()) {
            // A claim-bound connection whose launch ended attests nothing,
            // and it must never fall back to opening as `direct`.
            (None, false) => {
                self.observe_route_open_refusal(
                    ctx,
                    &caller,
                    &target_module_id,
                    "registration_channel_closed",
                    crate::supervise::REGISTRATION_CHANNEL_CLOSED,
                );
                return Ok(vec![control_error_frame(
                    &frame,
                    crate::supervise::REGISTRATION_CHANNEL_CLOSED,
                    "the claim binding's launch has ended",
                )?]);
            }
            _ => match self.route_open_principal_attested(
                &frame,
                consumer_identity,
                attested.as_ref().map(|attested| attested.principal.clone()),
            )? {
                Ok(principal) => principal,
                Err(error) => {
                    self.observe_route_open_refusal(
                        ctx,
                        &caller,
                        &target_module_id,
                        "bad_consumer_identity",
                        "bad_consumer_identity",
                    );
                    return Ok(vec![error]);
                }
            },
        };
        let attested = attested.expect("an attested principal was resolved above");
        timings.principal = principal_label(&principal);

        // The opener's spawn authority, read once here and carried with the
        // relay; nothing below reads its record again.
        let Ok(mut caller_capture) = attested.capture() else {
            self.observe_route_open_refusal(
                ctx,
                &caller,
                &target_module_id,
                "registration_channel_closed",
                crate::supervise::REGISTRATION_CHANNEL_CLOSED,
            );
            return Ok(vec![control_error_frame(
                &frame,
                crate::supervise::REGISTRATION_CHANNEL_CLOSED,
                "the opener's launch has ended",
            )?]);
        };
        // The caller-side checks below read the opener's own spawn: its
        // module from its record and its manifest from its own registration,
        // never another launch's active registration under the same id.
        let caller_module_id = match &attested.spawn {
            Some((record, _)) => Some(record.module_id().to_owned()),
            None => match &principal {
                Principal::Reserved { module_id } => Some(module_id.clone()),
                _ => None,
            },
        };

        // Only Callosum can attest a production phone build. Consume the raw
        // claim here; opted-in targets receive only the honoured peer fact.
        let build_class = identity.build_class.take().filter(|build_class| {
            if caller_module_id.as_deref() == Some("callosum") {
                build_class == subc_protocol::BUILD_CLASS_PRODUCTION
            } else {
                if self
                    .build_class_warned
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(ctx.connection_id)
                {
                    warn!(connection_id = ctx.connection_id.get(), principal = %timings.principal,
                        "ignoring build_class from a non-Callosum opener");
                }
                false
            }
        });

        // This is attested, control-plane policy for supervised module origins.
        // Keep it before route reservation and out of the opaque forwarding hot
        // path: data frames must never acquire a per-frame capability check.
        let opening_registration = self.caller_registration(&attested, &admission_registry);
        if let Some(opening_registration) = &opening_registration {
            let opening_module_id = opening_registration.module_id.as_str();
            if let Some(capability) = denied_capability(
                opening_registration.capabilities.as_ref(),
                registration.capabilities.as_ref(),
            ) {
                warn!(
                    opening_module_id,
                    target_module_id,
                    capability,
                    "refusing route.open because an attested capability deny edge matches"
                );
                return Ok(vec![self.route_open_refusal_frame(ctx, &caller,
                    &frame,
                    &target_module_id,
                    "capability_deny_edge",
                    "capability_forbidden",
                    format!(
                        "module_id '{opening_module_id}' must never reach capability '{capability}' provided by '{target_module_id}'"
                    ),
                )?]);
            }
        }

        if admission_facts.is_some() {
            let carrier_matches = caller_module_id.is_some()
                && self.admission_facts_carrier_module_id.as_deref() == caller_module_id.as_deref();
            if !carrier_matches {
                return Ok(vec![self.route_open_refusal_frame(
                    ctx,
                    &caller,
                    &frame,
                    &target_module_id,
                    "admission_facts_carrier_not_permitted",
                    "admission_facts_not_permitted",
                    "admission facts may only be carried by the configured reserved module",
                )?]);
            }

            let target_allowed = self
                .admission_facts_targets
                .as_ref()
                .is_some_and(|targets| targets.iter().any(|id| id == &target_module_id));
            if !target_allowed {
                return Ok(vec![self.route_open_refusal_frame(ctx, &caller,
                    &frame,
                    &target_module_id,
                    "admission_facts_target_not_listed",
                    "admission_facts_target_not_allowed",
                    format!(
                        "admission facts are not permitted for target module_id '{target_module_id}'"
                    ),
                )?]);
            }

            // Keep the value opaque to subc. The downstream admission validator owns
            // schema and semantic checks; this daemon only enforces carrier authority
            // and the configured destination allowlist.
        }

        // Scope admission, on the attested principal above and never on the
        // request body. The tag read here travels with the pending bind and is
        // compared with the published one at commit, so a sync between here
        // and the module's ack refuses the open instead of binding a stamp
        // that is no longer true.
        let (bound_scope, mut scope_stamp, stamp_required) = match &scope {
            None => (None, None, false),
            Some(selector) => {
                let owner_configured = match &selector.owner {
                    // A reserved owner counts as configured from before its
                    // process is spawned (see `SupervisorHandle::is_configured`).
                    // So an owner that has not synced its scopes yet is refused
                    // as retryable (`scope_not_synced`), not as one that will
                    // never sync.
                    Principal::Reserved { module_id } => self.supervisor.is_configured(module_id),
                    _ => false,
                };
                let mut scopes = self
                    .scopes
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let admitted = crate::scopes::admission::admit_route(
                    &scopes,
                    &admission_registry,
                    &principal,
                    &target_module_id,
                    Some(selector),
                    owner_configured,
                    crate::scopes::admission::AdmissionMode::Open,
                )
                .map(|route| {
                    let admission = route.scope.expect("scoped admission");
                    let bound = scopes.reserve_bind(
                        &admission,
                        &principal,
                        ctx.connection_id,
                        selector.hold_presence,
                    );
                    (admission, bound)
                });
                match admitted {
                    Ok((admission, bound)) => {
                        (Some(bound), Some(admission.stamp), admission.stamp_required)
                    }
                    Err(refusal) => {
                        return Ok(vec![self.route_open_refusal_frame(
                            ctx,
                            &caller,
                            &frame,
                            &target_module_id,
                            refusal.code.unwrap_or(error_codes::TARGET_UNAVAILABLE),
                            refusal.code.unwrap_or(error_codes::TARGET_UNAVAILABLE),
                            refusal.message,
                        )?]);
                    }
                }
            }
        };
        // From here on a refusal names the scope by the stamp admission
        // issued, exactly as the `route.open accepted` line does.
        if let Some(stamp) = &scope_stamp {
            caller.scope = route_open_scope_label(stamp);
        }
        // Decided from the admitted stamp, before any field of it is stripped
        // for a target that lacks the capability to read it.
        if scope_stamp.as_ref().is_some_and(scope_acts_for_another) {
            caller_capture = caller_capture.map(crate::spawn_authority::CallerCapture::relayed);
        }

        // Allow resolvable missing-tail paths so a vanished directory does not
        // prevent reopening an existing session for cleanup. Binding does not
        // authorize creation of durable provider state.
        //
        // Resolve real paths rather than lexical cleanup: canonicalize the
        // longest existing ancestor and append the missing tail, retaining the
        // path's real existing ancestry even when a leaf is missing.
        timings.enter(RouteOpenPhase::ProjectRoot);
        let legacy_project = match crate::bind_compat::LegacyProject::from_open(&frame.body) {
            Ok(project) => project,
            Err((code, message)) => return Ok(vec![control_error_frame(&frame, code, message)?]),
        };
        timings.enter(RouteOpenPhase::Admission);
        let project_root = match legacy_project.effective_root(scope_stamp.as_ref()) {
            Ok(root) => root,
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_project_root",
                    err.to_string(),
                )?])
            }
        };
        // Compute compatibility project fields before stripping optional stamp
        // members. Preserve the canonical opener root and its project ID,
        // including absence. For rootless openers, use stamp workdir or `/`, and
        // opener ID, else stamp ID, else no ID. Emit these identity fields only
        // to targets without bind-no-project/v1.
        let legacy_bind_project = legacy_project.for_target(scope_stamp.as_ref());

        // Last gate before any relay work, and deliberately after the cheap
        // registry and availability checks above: those name a more precise
        // condition (unknown, removed, reloading) and a caller is better served
        // by the precise code than by this one.
        //
        // Everything below this point costs an egress permit, a reserved handle
        // pair and, if the module does not answer, the whole relay budget. The
        // reader no longer waits for that budget, so cap each target explicitly;
        // serial dispatch used to provide the accidental cap of one relay per
        // connection. Admission is a mutex-protected count and never waits.
        let _concurrency_guard = match self
            .route_bind_concurrency
            .try_admit(&target_module_id, MAX_PENDING_ROUTE_BINDS_PER_TARGET)
        {
            Ok(guard) => guard,
            Err(in_flight) => {
                return Ok(vec![self.route_open_target_capacity_refusal(
                    ctx,
                    &caller,
                    &frame,
                    &target_module_id,
                    in_flight,
                )?]);
            }
        };

        // A module that has already burned the whole budget `threshold` times
        // in a row does not get to charge it again until a probe says it recovered.
        let mut breaker = match self.route_bind_breakers.admit(&target_module_id) {
            RouteBindAdmission::Admitted { guard, probe } => {
                if probe {
                    info!(
                        module_id = %target_module_id,
                        connection_id = ctx.connection_id.get(),
                        "route.bind breaker half-open: admitting one probe"
                    );
                }
                guard
            }
            RouteBindAdmission::Refused {
                consecutive_timeouts,
                retry_in,
                probe_in_flight,
            } => {
                return Ok(vec![self.route_open_breaker_refusal_frame(
                    ctx,
                    &caller,
                    &frame,
                    &target_module_id,
                    consecutive_timeouts,
                    retry_in,
                    probe_in_flight,
                )?]);
            }
        };

        let operator_app = self
            .route_operator_app(ctx.connection_id, registration.capabilities.as_ref())
            .await;
        // Resolve the per-module budget here so the wait matches the operator's
        // intent for this specific target. A per-module override in
        // `subc.jsonc` (or `with_route_bind_relay_timeouts` for embedded
        // daemons) wins over the daemon-wide default.
        let route_bind_relay_timeout = self.route_bind_relay_timeout_for(&target_module_id);
        let relay_deadline = Instant::now() + route_bind_relay_timeout;
        let production_peer_build =
            build_class.as_deref() == Some(subc_protocol::BUILD_CLASS_PRODUCTION);
        timings.enter(RouteOpenPhase::EgressReservation);
        let pending = match self
            .forwarding
            .begin_route_bind_relay_for(
                ctx.connection_id,
                ctx.egress.clone(),
                response_version(&frame),
                frame.header.corr,
                &target_module_id,
                principal.clone(),
                bound_scope,
                project_root,
                identity.harness.clone(),
                build_class,
                operator_app.clone(),
                crate::forwarding::RouteAuthority {
                    incarnation: self.supervisor.daemon_incarnation(),
                    caller: caller_capture
                        .as_ref()
                        .map(|caller| caller.authority.pair()),
                    capability_fence: route_bind_settlement::fence(
                        &opening_registration,
                        registration,
                    ),
                },
                relay_deadline,
                busy_refusals == Some(true),
                accepts_close_reasons,
            )
            .await
        {
            Ok(pending) => pending,
            Err(err) => {
                return Ok(vec![self.route_open_refusal_frame(
                    ctx,
                    &caller,
                    &frame,
                    &target_module_id,
                    "relay_reservation_failed",
                    forwarding_error_code(&err),
                    err.to_string(),
                )?])
            }
        };
        timings.enter(RouteOpenPhase::Admission);
        let crate::forwarding::PendingRouteBindRelay {
            endpoint,
            module_sink,
            negotiated_ver,
            client_channel,
            client_epoch,
            module_channel,
            module_epoch,
            corr: relay_corr,
            receiver,
        } = pending;
        let mut reservation =
            RouteBindReservationGuard::new(Arc::clone(&self.forwarding), endpoint, relay_corr);

        // Reserving egress can wait while a module reconnects or a swap cuts
        // over. Check the relay's captured connection, not the earlier by-id
        // lookup: the original module's flow/run capabilities cannot authorize
        // its replacement. The captured sink stays bound to that connection.
        let relay_registration = self
            .registry
            .route_registration_by_connection(endpoint.connection_id)
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?;
        let relay_capabilities = relay_registration
            .as_ref()
            .and_then(|registration| registration.capabilities.as_ref());
        if let Some(selector) = &scope {
            // Egress reservation may wait across a scope update. Recheck the
            // proof against the current record before sending the captured bind.
            if let Err(refusal) = self
                .scopes
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .check_bind_proof(&principal, selector)
            {
                reservation.release_and_disarm();
                return Ok(vec![self.route_open_refusal_frame(
                    ctx,
                    &caller,
                    &frame,
                    &target_module_id,
                    refusal.code,
                    refusal.code,
                    refusal.message,
                )?]);
            }
        }
        if let Some(stamp) = scope_stamp.as_ref() {
            if let Err(refusal) = crate::scopes::check_target_required_stamp(
                stamp_required,
                &target_module_id,
                relay_capabilities,
            )
            .and_then(|()| {
                crate::scopes::check_target_flow_support(
                    stamp,
                    &target_module_id,
                    relay_capabilities,
                )
            })
            .and_then(|()| {
                crate::scopes::check_target_agent_run_support(
                    stamp,
                    &target_module_id,
                    relay_capabilities,
                )
            }) {
                reservation.release_and_disarm();
                return Ok(vec![self.route_open_refusal_frame(
                    ctx,
                    &caller,
                    &frame,
                    &target_module_id,
                    refusal.code,
                    refusal.code,
                    refusal.message,
                )?]);
            }
        }

        // Use the admitted scope before compatibility removes fields from the bind.
        let caller_attestation = self.caller_for_target(
            caller_capture.as_ref(),
            relay_capabilities,
            &target_module_id,
            &principal,
            scope_stamp
                .as_ref()
                .and_then(|stamp| stamp.project_id.as_deref()),
        );
        // Older readers may reject unknown fields. Select optional fields from
        // the captured endpoint's declaration, including on unscoped binds.
        if scope_stamp
            .as_ref()
            .is_some_and(|stamp| stamp.kind == subc_protocol::scope::ScopeKind::Federated)
            && !crate::scopes::target_supports_federated(relay_capabilities)
        {
            // Keep bound_scope: even an unstamped legacy route belongs to this epoch.
            scope_stamp = None;
        }
        if !relay_capabilities.is_some_and(|caps| {
            caps.provides
                .iter()
                .any(|cap| cap == subc_protocol::scope::SCOPE_WORKDIR_CAPABILITY)
        }) {
            if let Some(stamp) = scope_stamp.as_mut() {
                stamp.workdir = None;
                stamp.project_id = None;
            }
        }
        // A worker run has no agent_id; on_behalf_of_agent is its only agent
        // attribution. The run gate above has already required the target to
        // declare `worker-runs/v1`, which promises to read both
        // `on_behalf_of_agent` and `call_mode`.
        let worker_run = scope_stamp
            .as_ref()
            .is_some_and(crate::scopes::is_worker_run)
            && relay_capabilities.is_some_and(|caps| {
                caps.provides
                    .iter()
                    .any(|cap| cap == subc_protocol::scope::WORKER_RUNS_CAPABILITY)
            });
        if !worker_run
            && !relay_capabilities.is_some_and(|caps| {
                caps.provides
                    .iter()
                    .any(|cap| cap == subc_protocol::scope::SCOPE_ON_BEHALF_OF_CAPABILITY)
            })
        {
            if let Some(stamp) = scope_stamp.as_mut() {
                stamp.attributes.on_behalf_of_agent = None;
            }
        }
        if !relay_capabilities.is_some_and(|caps| {
            caps.provides
                .iter()
                .any(|cap| cap == subc_protocol::scope::FLOW_IDENTITY_SCOPES_CAPABILITY)
        }) {
            if let Some(stamp) = scope_stamp.as_mut() {
                stamp.attributes.flow_version = None;
                stamp.attributes.flow_code_hash = None;
                stamp.attributes.flow_install_id = None;
            }
        }
        let call_mode = if worker_run
            || relay_capabilities
                .is_some_and(|caps| caps.provides.iter().any(|cap| cap == CALL_MODES_CAPABILITY))
        {
            Some(derive_call_mode(scope_stamp.as_ref()))
        } else {
            if let Some(stamp) = scope_stamp.as_mut() {
                stamp.expires_at_ms = None;
            }
            None
        };

        debug!(
            connection_id = ctx.connection_id.get(),
            client_channel,
            client_epoch,
            module_channel,
            module_epoch,
            "reserved route handle pair"
        );
        // Rendered BEFORE the move into the relay, because the accept arm below
        // is where it is logged and the principal is gone by then.
        let route_principal_label = principal_label(&principal);
        let relay = ModuleControlRequest::RouteBind {
            route_channel: module_channel,
            epoch: module_epoch,
            target,
            identity,
            principal: Some(principal),
            consumer_capabilities,
            role_versions,
            admission_facts,
            scope: scope_stamp,
            call_mode,
        };
        let peer = RouteBindPeer::new()
            .with_observed_peer(
                relay_capabilities
                    .is_some_and(|caps| {
                        caps.provides
                            .iter()
                            .any(|cap| cap == OBSERVED_PEER_CAPABILITY)
                    })
                    .then(|| {
                        self.observed_peers
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .get(&ctx.connection_id)
                            .cloned()
                            .unwrap_or_else(ObservedPeer::unobserved)
                    }),
            )
            .with_operator_app(
                self.connection_operator_app(operator_app.as_ref())
                    .filter(|_| {
                        relay_capabilities.is_some_and(|caps| {
                            caps.provides
                                .iter()
                                .any(|cap| cap == OPERATOR_APP_CAPABILITY)
                        })
                    }),
            )
            .with_caller(caller_attestation)
            .with_peer_build(
                (production_peer_build
                    && relay_capabilities.is_some_and(|caps| {
                        caps.provides.iter().any(|cap| cap == PEER_BUILD_CAPABILITY)
                    }))
                .then(|| subc_protocol::BUILD_CLASS_PRODUCTION.to_owned()),
            );
        let relay_body = crate::bind_compat::encode_bind(
            &relay,
            &peer,
            &legacy_bind_project,
            relay_capabilities,
        )
        .map_err(|err| {
            RouterError::backend(
                0,
                frame.header.corr,
                format!("failed to encode route.bind request: {err}"),
            )
        })?;
        let relay_frame = Frame::build_with_version(
            negotiated_ver,
            FrameType::Request,
            control_flags(),
            0,
            0,
            relay_corr,
            relay_body,
        )
        .map_err(RouterError::FrameBuild)?;

        timings.enter(RouteOpenPhase::BindSend);
        // Enqueueing the bind spends the same absolute budget as its ack.
        let sent = timeout_at(relay_deadline, module_sink.send(relay_frame)).await;
        timings.enter(RouteOpenPhase::Admission);
        let Ok(sent) = sent else {
            // Never enqueued: the module's queue, not its bind handler, was
            // slow, so the ack-timeout breaker is not charged.
            reservation.release_and_disarm();
            breaker.record_inconclusive();
            return Ok(vec![self.unsent_bind_timeout_refusal(
                ctx,
                &caller,
                &frame,
                &target_module_id,
            )?]);
        };
        if let Err(err) = sent {
            reservation.release_and_disarm();
            return Ok(vec![self.route_open_refusal_frame(
                ctx,
                &caller,
                &frame,
                &target_module_id,
                "relay_send_failed",
                "target_unavailable",
                err.to_string(),
            )?]);
        }

        #[cfg(test)]
        if let Some(hold) = &self.route_bind_relay_sent_hold {
            // First wait: tell the test the bind is queued. Second wait: stay
            // here until the test has driven whatever it wants to happen
            // between queueing the bind and marking it enqueued.
            hold.wait().await;
            hold.wait().await;
        }

        // A missing relay is not by itself an abandoned bind: the module may
        // already have acked it and the route may be committed. Only a relay
        // dropped without any module reply needs this GOODBYE; one a reply
        // settled has already had its GOODBYE decided by the reply path.
        if self
            .forwarding
            .mark_route_bind_relay_enqueued(endpoint, relay_corr)
            .map_err(RouterError::Forwarding)?
            == crate::forwarding::RouteBindRelayMark::ReleasedWithoutReply
        {
            self.send_abandoned_route_bind_goodbye(
                &module_sink,
                negotiated_ver,
                module_channel,
                module_epoch,
            );
        }

        timings.enter(RouteOpenPhase::BindAckWait);
        let outcome = timeout_at(relay_deadline, receiver).await;
        timings.enter(RouteOpenPhase::Admission);
        match outcome {
            Ok(Ok(RouteBindRelayOutcome::Accepted)) => {
                reservation.disarm();
                if breaker.record_accepted() {
                    info!(
                        module_id = %target_module_id,
                        "route.bind breaker closed: the probe was accepted"
                    );
                }
                self.observe_route_open_accept(
                    ctx,
                    &target_module_id,
                    &route_principal_label,
                    &caller.scope,
                );
                Ok(Vec::new())
            }
            Ok(Ok(RouteBindRelayOutcome::Rejected(body))) => {
                reservation.release_and_disarm();
                // A module that says no in microseconds is healthy. Rejection
                // is a different condition with its own refusal and must not
                // move the breaker.
                breaker.record_inconclusive();
                self.route_bind_rejection(ctx, &caller, &frame, &target_module_id, body, false)
            }
            Ok(Ok(RouteBindRelayOutcome::DaemonRejected(body))) => {
                reservation.release_and_disarm();
                breaker.record_inconclusive();
                self.route_bind_rejection(ctx, &caller, &frame, &target_module_id, body, true)
            }
            Ok(Ok(RouteBindRelayOutcome::ModuleGone(message))) => {
                reservation.release_and_disarm();
                breaker.record_inconclusive();
                // Fires when the module's connection closes while a relayed
                // bind is pending -- typically a caller racing a module restart
                // whose bind was relayed BEFORE the drain mark went up. Logged
                // because the caller sees only its own error and the fleet has
                // already spent one diagnosis round unable to tell this arm
                // from a relay timeout without daemon-side evidence.
                tracing::warn!(
                    module_id = %target_module_id,
                    "route.bind relay abandoned: {message}"
                );
                Ok(vec![self.route_open_refusal_frame(
                    ctx,
                    &caller,
                    &frame,
                    &target_module_id,
                    "relay_abandoned",
                    "target_unavailable",
                    message,
                )?])
            }
            Ok(Err(_)) => {
                reservation.release_and_disarm();
                breaker.record_inconclusive();
                Ok(vec![self.route_open_refusal_frame(
                    ctx,
                    &caller,
                    &frame,
                    &target_module_id,
                    "relay_waiter_canceled",
                    "target_unavailable",
                    "route.bind relay waiter was canceled before the module responded",
                )?])
            }
            Err(_) => {
                reservation.release_and_disarm();
                // THE ONLY ARM THAT MOVES THE BREAKER. Budget exhausted with no
                // answer at all is the one condition a fast refusal can
                // usefully stand in for; every other arm already answered.
                if let Some(opened) = breaker.record_timeout(
                    self.route_bind_breaker_threshold,
                    self.route_bind_breaker_cooldown,
                ) {
                    warn!(
                        module_id = %target_module_id,
                        consecutive_timeouts = opened.consecutive_timeouts,
                        cooldown_ms = self.route_bind_breaker_cooldown.as_millis() as u64,
                        reopened_after_probe = opened.reopened_after_probe,
                        "route.bind breaker open: refusing route.open for this module without relaying until one probe says it recovered"
                    );
                }
                // The generous budget just burned to no answer: the module is
                // registered and its connection is up, but its bind handler sat
                // on the ack for the full budget (warm-on-bind, cold configure,
                // or a wedged handler). Every earlier unavailability shape
                // fast-refuses BEFORE the relay, so this arm firing means the
                // slowness is module-side -- log it so the per-module timeline
                // is reconstructable without client audit rows.
                tracing::warn!(
                    module_id = %target_module_id,
                    timeout_ms = route_bind_relay_timeout.as_millis() as u64,
                    "route.bind relay timed out: module did not ack within budget"
                );
                Ok(vec![self.route_open_refusal_frame(
                    ctx,
                    &caller,
                    &frame,
                    &target_module_id,
                    "relay_timed_out",
                    "module_timeout",
                    format!(
                        "module_id '{target_module_id}' did not answer route.bind within {:?}",
                        route_bind_relay_timeout
                    ),
                )?])
            }
        }
    }
}

fn target_module_id(target: &RouteTarget) -> &str {
    match target {
        RouteTarget::ToolProvider { module_id }
        | RouteTarget::ManagementSurface { module_id }
        | RouteTarget::InternalService { module_id, .. } => module_id,
    }
}

fn target_has_required_role(
    target: &RouteTarget,
    roles: &[crate::registry::AdmissionRole],
) -> bool {
    use crate::registry::AdmissionRole;
    roles.iter().any(|role| match (target, role) {
        (RouteTarget::ToolProvider { .. }, AdmissionRole::ToolProvider) => true,
        (RouteTarget::ManagementSurface { .. }, AdmissionRole::ManagementSurface) => true,
        (
            RouteTarget::InternalService { service_id, .. },
            AdmissionRole::InternalService(provided),
        ) => service_id == provided,
        _ => false,
    })
}

pub(super) fn is_routable_role(role: &ProviderRole) -> bool {
    matches!(
        role,
        ProviderRole::ToolProvider { .. }
            | ProviderRole::ManagementSurface { .. }
            | ProviderRole::InternalService { .. }
    )
}

/// The caller of one `route.open`, as its `route.open refused` line names it.
///
/// A connection id alone cannot name a caller after the fact: ids restart at
/// every daemon restart, so the same number belongs to different modules
/// before and after one. These three fields are what the refused line needs
/// to be attributable on its own, and they are rendered with the same helpers
/// as the `route.open accepted` line so both directions grep alike.
pub(super) struct RouteOpenCaller {
    /// The attested principal (`direct`, `reserved:<module>`), or `unattested`
    /// when the request presented a consumer identity whose launch nonce did
    /// not verify. The claimed module id is never printed as a principal.
    pub(super) principal: String,
    /// `none` for an unscoped open; the admitted stamp's label once scope
    /// admission issued one; before that, the requested selector's label.
    pub(super) scope: String,
    /// The bind identity's harness, Debug-quoted and bounded like a scope ref,
    /// because it is a string the caller chose.
    pub(super) harness: String,
}

impl RouteOpenCaller {
    fn new(
        principal: Option<&Principal>,
        scope: Option<&ScopeSelector>,
        identity: &BindIdentity,
    ) -> Self {
        Self {
            principal: principal.map_or_else(|| "unattested".to_string(), principal_label),
            scope: scope.map_or_else(|| "none".to_string(), route_open_requested_scope_label),
            harness: bounded_log_text(&identity.harness),
        }
    }

    /// For a refusal that has no parsed request at hand: names whatever the
    /// frame still says, and `unknown` where it says nothing parseable.
    pub(super) fn unknown() -> Self {
        Self {
            principal: "unknown".to_string(),
            scope: "unknown".to_string(),
            harness: "unknown".to_string(),
        }
    }
}

/// Longest client-chosen text (a scope ref, a harness) a route.open log line
/// prints before cutting it.
const MAX_LOGGED_TEXT_CHARS: usize = 64;

/// `value` Debug-quoted, so control characters land escaped and cannot forge
/// a log line, and cut to [`MAX_LOGGED_TEXT_CHARS`] characters with a trailing
/// `…` inside the quotes so one request cannot flood the line.
fn bounded_log_text(value: &str) -> String {
    let mut bounded: String = value.chars().take(MAX_LOGGED_TEXT_CHARS).collect();
    if value.chars().nth(MAX_LOGGED_TEXT_CHARS).is_some() {
        bounded.push('…');
    }
    format!("{bounded:?}")
}

/// The `scope=` field of the `route.open accepted` line, from the stamp that
/// admission issued: `<kind>/<owner>/<ref>@<epoch>`. The ref is Debug-quoted
/// so a ref holding control characters cannot forge a log line, and cut to 64
/// characters with a trailing `…` to bound the line. The bind proof and the
/// scope's attributes never appear.
fn route_open_scope_label(stamp: &subc_protocol::scope::ScopeStamp) -> String {
    use subc_protocol::scope::ScopeKind;
    let kind = match stamp.kind {
        ScopeKind::Head => "head",
        ScopeKind::Worker => "worker",
        ScopeKind::Ephemeral => "ephemeral",
        ScopeKind::Federated => "federated",
        _ => "unknown",
    };
    format!(
        "{kind}/{}/{}@{}",
        principal_label(&stamp.owner),
        bounded_log_text(&stamp.scope_ref),
        stamp.scope_epoch
    )
}

/// The `scope=` field of a `route.open refused` line for an open refused
/// before scope admission issued a stamp: `requested/<owner>/<ref>@<epoch>`.
/// The kind is not known yet, so the literal `requested` stands in its place.
/// The owner is Debug-quoted as well as the ref, because unlike a stamp's
/// owner it has not been checked against the configured modules: a quoted
/// owner means the value was never validated. The bind proof never appears.
fn route_open_requested_scope_label(selector: &ScopeSelector) -> String {
    let epoch = selector
        .scope_epoch
        .map_or_else(|| "none".to_string(), |epoch| epoch.to_string());
    format!(
        "requested/{}/{}@{epoch}",
        bounded_log_text(&principal_label(&selector.owner)),
        bounded_log_text(&selector.scope_ref),
    )
}

pub(super) fn principal_label(principal: &Principal) -> String {
    match principal {
        Principal::Reserved { module_id } => format!("reserved:{module_id}"),
        Principal::Direct => "direct".to_string(),
        other => format!("{other:?}"),
    }
}
