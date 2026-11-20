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
