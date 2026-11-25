use super::*;

impl SupervisorSnapshot {
    pub(super) fn crash_restarts_in_window(&mut self, window: Duration, now: Instant) -> u32 {
        while self
            .crash_restarts
            .front()
            .is_some_and(|oldest| now.duration_since(*oldest) > window)
        {
            self.crash_restarts.pop_front();
        }
        u32::try_from(self.crash_restarts.len()).unwrap_or(u32::MAX)
    }

    /// Count the replacement only once a child was actually spawned.
    pub(super) fn record_crash_restart(&mut self, policy: &RestartPolicy, spawned_at: Instant) {
        self.crash_restarts_in_window(policy.window, spawned_at);
        self.crash_restarts.push_back(spawned_at);
        while self.crash_restarts.len() > policy.max_restarts as usize {
            self.crash_restarts.pop_front();
        }
        self.lifetime_restarts += 1;
    }

    /// Authorize a recovery and choose its backoff without counting it as a spawn.
    pub(super) fn next_crash_restart(
        &mut self,
        policy: &RestartPolicy,
        now: Instant,
    ) -> Option<CrashRestartSchedule> {
        let restart_in_window = self.crash_restarts_in_window(policy.window, now);
        (restart_in_window < policy.max_restarts).then(|| CrashRestartSchedule {
            restart_in_window,
            delay: policy.delay_for_restart(restart_in_window),
        })
    }
}

impl RespawnKind {
    pub(super) fn after_exit(schedule: Option<CrashRestartSchedule>) -> Self {
        if schedule.is_some() {
            Self::CrashRestart
        } else {
            Self::Spawn
        }
    }
}

pub(super) async fn spawn_and_mark_running_async(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    snapshot: &SharedSnapshot,
) -> Result<SupervisedChild, SuperviseError> {
    admit_running_child_async(spec, runtime, snapshot, RespawnKind::Spawn).await
}

pub(super) async fn admit_running_child_async(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    snapshot: &SharedSnapshot,
    kind: RespawnKind,
) -> Result<SupervisedChild, SuperviseError> {
    let activity_spawn = runtime.begin_activity_spawn(&spec.module_id);
    let registration_watch = watch_registration(&runtime.registry, spec);
    #[cfg(unix)]
    let joining_group = containment::current(&runtime.child_roster.containment)
        .is_some_and(|container| container.group().is_some());
    let result = spawn_child_async(spec, runtime).await;
    #[cfg(unix)]
    let result = match result {
        Err(error)
            if joining_group
                && !is_launch_deferral(&error)
                && !runtime.child_roster.is_closed()
                && lock_snapshot(snapshot).is_ok_and(|state| state.enabled)
                && runtime
                    .child_roster
                    .previous_instance(&spec.module_id)
                    .is_none() =>
        {
            // The failed join ended the old container. The single retry in a
            // fresh container still belongs to this recovery.
            warn!(module_id = %spec.module_id, %error, "carried process-group spawn failed; trying one fresh container");
            spawn_child_async(spec, runtime).await
        }
        other => other,
    };
    let mut child = result?;
    if matches!(kind, RespawnKind::CrashRestart) {
        let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
            state.record_crash_restart(&runtime.restart_policy, child.spawned_at);
        });
    }
    arm_registration_deadline(&mut child, runtime.register_timeout, registration_watch);
    if let Err(error) = set_running(snapshot, &child, &spec.module_id, &runtime.spawn_events) {
        containment::terminate_blocking(&runtime.child_roster.containment);
        return Err(error);
    }
    if let Some(spawn) = activity_spawn {
        spawn.complete(snapshot);
    }
    Ok(child)
}

const DEFAULT_MAX_RESTARTS: u32 = 3;
const DEFAULT_BACKOFF: Duration = Duration::from_millis(100);
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(30);
/// The span `DEFAULT_MAX_RESTARTS` is counted over. Ten minutes is long enough
/// to contain a real crash loop (which respawns in seconds) and short enough
/// that unrelated crashes hours apart never accumulate into a permanent stop.
const DEFAULT_RESTART_WINDOW: Duration = Duration::from_secs(600);

/// Bounded restart policy for crash exits.
///
/// `max_restarts` is the number of replacement processes allowed after the
/// initial spawn WITHIN `window`. After that many crash restarts inside one
/// window the module enters [`ModuleState::Failed`] and the supervisor stops
/// the crash loop.
///
/// The budget is a RATE, not a lifetime total. It used to be a lifetime total,
/// and that only survived because crashes were rare: a module that crashed
/// three times across a week was disabled forever by crashes that had nothing
/// to do with each other. That stopped being survivable once modules began
/// exiting non-zero whenever the daemon's connection to them drops, because
/// then every daemon-side connection drop spends a unit of the same budget and
/// one flappy hour permanently stops a healthy module. Restarts older than
/// `window` release their slot, so a module that crashed twice yesterday has a
/// full budget today, while a genuine crash loop -- which is fast by
/// definition -- still reaches the cap and stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    pub max_restarts: u32,
    /// Base delay before a crash replacement. The actual delay escalates with
    /// the number of recent crash replacements and is capped by `max_backoff`.
    pub backoff: Duration,
    /// Maximum delay before a crash replacement.
    pub max_backoff: Duration,
    /// The span `max_restarts` is counted over. `Duration::ZERO` makes the
    /// budget effectively infinite (nothing is ever in-window), which is why
    /// daemon config refuses `window_secs: 0` rather than quietly accepting it.
    pub window: Duration,
}

impl RestartPolicy {
    /// A policy with the default crash window. Callers that care about the
    /// window say so with [`Self::with_window`]; the ones that do not are
    /// asking for the standard rate limit, not for no limit.
    pub fn new(max_restarts: u32, backoff: Duration) -> Self {
        Self {
            max_restarts,
            backoff,
            max_backoff: DEFAULT_MAX_BACKOFF,
            window: DEFAULT_RESTART_WINDOW,
        }
    }

    pub fn with_max_backoff(mut self, max_backoff: Duration) -> Self {
        self.max_backoff = max_backoff;
        self
    }

    pub fn with_window(mut self, window: Duration) -> Self {
        self.window = window;
        self
    }

    /// Calculate the capped exponential delay for the next crash replacement.
    /// `restart_in_window` is zero for the first replacement after an operator
    /// action (restart, reload, re-enable) cleared the crash ring, or after all
    /// older crash replacements have aged out of the window.
    pub(super) fn delay_for_restart(&self, restart_in_window: u32) -> Duration {
        if self.backoff.is_zero() || self.max_backoff.is_zero() {
            return Duration::ZERO;
        }

        let mut delay = self.backoff;
        for _ in 0..restart_in_window {
            if delay >= self.max_backoff {
                return self.max_backoff;
            }
            delay = delay
                .checked_mul(10)
                .unwrap_or(self.max_backoff)
                .min(self.max_backoff);
        }
        delay.min(self.max_backoff)
    }

    /// The one sentence that explains a budget-exhausted stop, used for both the
    /// log line and the terminal record so the two cannot drift. It names the
    /// window because `max_restarts=3` alone reads as a lifetime cap, which is
    /// exactly what this budget is not.
    pub(super) fn budget_exhausted_detail(&self) -> String {
        format!(
            "crash budget exhausted: max_restarts={} within window_secs={}",
            self.max_restarts,
            self.window.as_secs()
        )
    }
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_restarts: DEFAULT_MAX_RESTARTS,
            backoff: DEFAULT_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
            window: DEFAULT_RESTART_WINDOW,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CrashRestartSchedule {
    pub(super) restart_in_window: u32,
    pub(super) delay: Duration,
}

/// Whether the daemon itself will bring this module back after the exit being
/// handled: it is enabled AND its in-window crash restarts are below the cap.
///
/// Takes `&mut` because reading the budget prunes it. Instants that fell out of
/// the window are dropped here rather than by a timer, so the count is right
/// the moment somebody asks and no bookkeeping runs for idle modules.
pub(super) fn daemon_will_restart(
    state: &mut SupervisorSnapshot,
    policy: &RestartPolicy,
    now: Instant,
) -> bool {
    state.enabled && state.crash_restarts_in_window(policy.window, now) < policy.max_restarts
}

impl SupervisedModule {
    #[cfg(all(test, unix))]
    pub(crate) fn seed_crash_budget_for_test(&self, count: u32) {
        let policy = self.inner.configuration.lock().unwrap().restart_policy;
        let mut state = self.inner.snapshot.lock().unwrap();
        for _ in 0..count {
            state.record_crash_restart(&policy, Instant::now());
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn budgeted_restart_child(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<SupervisedChild>,
    status: SupervisorHealthStatus,
    detail: Option<&str>,
    now_ms: u64,
    health_failure: bool,
) -> Result<(), SuperviseError> {
    if let Some(error) = runtime
        .config_delivery
        .as_ref()
        .and_then(|delivery| delivery.0.check_module_hold(&spec.module_id).err())
        .filter(|error| error.code != "config_delivery_deferred")
    {
        return Err(config_delivery::gate_hold_error(
            spec,
            error,
            runtime.config_delivery.as_ref(),
        ));
    }
    let recovery_reason = if health_failure {
        "health_check"
    } else {
        "configuration_delivery"
    };
    let (enabled, schedule) = {
        let mut state = lock_snapshot(snapshot)?;
        let enabled = state.enabled;
        let schedule = if enabled {
            state.next_crash_restart(&runtime.restart_policy, Instant::now())
        } else {
            None
        };
        (enabled, schedule)
    };

    if !enabled {
        return Err(SuperviseError::Disabled {
            module_id: spec.module_id.clone(),
        });
    }

    if schedule.is_none() {
        if health_failure {
            record_health_action(snapshot, &spec.module_id, "failed".to_string(), now_ms);
        }
        error!(
            module_id = %spec.module_id,
            status = ?status,
            detail,
            max_restarts = runtime.restart_policy.max_restarts,
            window_secs = runtime.restart_policy.window.as_secs(),
            reason = %runtime.restart_policy.budget_exhausted_detail(),
            recovery_reason,
            "automatic restart budget exhausted; marking module failed"
        );
        let stop_notice = begin_forwarding_drain_if_configured(
            spec,
            runtime,
            registry,
            snapshot,
            Some(true),
            RouteCloseReason::Disable,
        )
        .await?;
        update_snapshot(snapshot, Some(&spec.module_id), |state| {
            state.drain_disposition_detail = Some(runtime.restart_policy.budget_exhausted_detail());
        })?;
        let result = drain_optional_child(
            &spec.module_id,
            spec.protocol,
            stop_notice,
            registry,
            runtime.forwarding.as_deref(),
            snapshot,
            &runtime.terminal_ring,
            &runtime.spawn_events,
            child,
            runtime.drain_timeout,
            ModuleState::Failed,
            Some(true),
            &runtime.failure_context(true),
        )
        .await;
        update_snapshot(snapshot, Some(&spec.module_id), |state| {
            if state.state == ModuleState::Failed && !runtime.child_roster.is_closed() {
                state.failed_retry.schedule(&spec.module_id);
            }
        })?;
        result?;
        process_liveness.untrack_if_current(&spec.module_id, snapshot);
        return Ok(());
    }

    let schedule = schedule.expect("a health restart must have a crash-restart schedule");
    let mut restart_count = 0;
    update_snapshot(snapshot, Some(&spec.module_id), |state| {
        restart_count = state.crash_restarts.len();
        state.state = ModuleState::Unresponsive;
        if health_failure {
            state.health.status = status;
            state.health.last_action = Some(HealthAction::Restart.to_string());
            state.health.last_action_ms = Some(now_ms);
        }
    })?;
    warn!(
        module_id = %spec.module_id,
        status = ?status,
        detail,
        restart_count,
        restart_in_window = schedule.restart_in_window,
        delay_ms = schedule.delay.as_millis() as u64,
        recovery_reason,
        "automatic module restart"
    );

    let stop_notice = begin_forwarding_drain_if_configured(
        spec,
        runtime,
        registry,
        snapshot,
        Some(true),
        RouteCloseReason::Restart,
    )
    .await?;
    if stop_notice == StopNotice::DaemonShutdown {
        return Ok(());
    }
    drain_optional_child(
        &spec.module_id,
        spec.protocol,
        stop_notice,
        registry,
        runtime.forwarding.as_deref(),
        snapshot,
        &runtime.terminal_ring,
        &runtime.spawn_events,
        child,
        runtime.drain_timeout,
        ModuleState::Restarting,
        Some(true),
        &runtime.failure_context(true),
    )
    .await?;
    schedule_respawn(
        runtime,
        snapshot,
        &spec.module_id,
        schedule.delay,
        RespawnKind::CrashRestart,
        true,
    )
}

pub(super) fn schedule_respawn(
    runtime: &SupervisorRuntimeConfig,
    snapshot: &SharedSnapshot,
    module_id: &str,
    delay: Duration,
    kind: RespawnKind,
    daemon_initiated: bool,
) -> Result<(), SuperviseError> {
    cancel_deferred_reload(runtime, module_id, "respawn superseded by another restart");
    update_snapshot(snapshot, Some(module_id), |state| {
        state.respawn_pending = true
    })?;
    *runtime
        .scheduled_respawn
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = Some(PendingRespawn {
        deadline: Instant::now() + delay,
        kind,
        daemon_initiated,
        approval: None,
    });
    Ok(())
}

pub(super) fn log_crash_respawn(module_id: &str, schedule: CrashRestartSchedule) {
    info!(
        module_id,
        restart_in_window = schedule.restart_in_window,
        delay_ms = schedule.delay.as_millis() as u64,
        "respawning after crash"
    );
}

/// Whether the respawn a backoff was counting down to is still wanted. A
/// disable or drain handled while the backoff elapsed moves the snapshot out
/// of `Restarting`, and the operator's stop must win over the pending respawn,
/// so every sleep-then-spawn path re-validates against the live snapshot
/// instead of assuming the state it left behind still holds.
pub(super) fn respawn_still_pending(snapshot: &SharedSnapshot) -> bool {
    matches!(
        lock_snapshot(snapshot),
        Ok(state) if state.enabled && state.state == ModuleState::Restarting
    )
}

/// Give an operator-touched module its full crash budget back.
///
/// Named for the counter it used to zero; it now empties the in-window ring,
/// which is the same act. `lifetime_restarts` is untouched on purpose -- the
/// ledger of what happened survives every operator action.
pub(super) fn reset_restart_count(
    snapshot: &SharedSnapshot,
    module_id: &str,
) -> Result<(), SuperviseError> {
    update_snapshot(snapshot, Some(module_id), |state| {
        state.clear_crash_restarts();
    })
}
