//! Owned tasks isolate each module's outbound delivery and acknowledgment deadlines.

use super::*;
use crate::config_gate::runtime::{
    delivery::CONFIG_LIVE_ACK_TIMEOUT, AppliedDisposition, DeliveryObserver,
};
use std::collections::{HashMap, HashSet};
use tokio::sync::Notify;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{sleep_until, timeout_at, Instant};

#[derive(Default)]
struct AuthorityExitRetry {
    generation: Option<u64>,
    next: Option<Instant>,
    delay: Duration,
}

impl AuthorityExitRetry {
    fn observe_pending(&mut self, generation: Option<u64>, now: Instant) {
        if generation.is_none() {
            *self = Self::default();
        } else if self.generation != generation {
            self.generation = generation;
            self.delay = Duration::from_secs(1);
            self.next = Some(now + self.delay);
        } else if self.next.is_some_and(|deadline| deadline <= now) {
            self.delay = (self.delay * 2).min(Duration::from_secs(60));
            self.next = Some(now + self.delay);
        }
        // Configuration-state notifications can retry immediately, but do not
        // move an outstanding deadline or postpone timed retries.
    }
}

impl ControlHandler {
    #[cfg(all(test, unix))]
    pub(crate) async fn record_config_exit_for_test(
        self: &Arc<Self>,
        exited: crate::config_gate::runtime::delivery::ExitedDelivery,
    ) -> bool {
        record_exit(Arc::downgrade(self), exited).await
    }

    /// The bootstrap task owns this handle and aborts it at daemon teardown.
    /// The configuration delivery coordinator and its idle module tasks hold
    /// weak control-handler references, so they cannot retain the config store's
    /// writer lock held for the configuration store after the daemon is dropped.
    pub(crate) fn spawn_config_delivery_worker(self: &Arc<Self>) -> Option<JoinHandle<()>> {
        let owner = self.config_runtime.as_ref()?;
        let changed = owner.delivery_notifications();
        let handler = Arc::downgrade(self);
        Some(tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            let mut wakes = HashMap::<ConnectionId, Arc<Notify>>::new();
            let recording = Arc::new(std::sync::Mutex::new(HashSet::<String>::new()));
            let mut restarting = HashSet::<(String, u64)>::new();
            let mut recovery_holds = HashSet::<String>::new();
            let mut exit_retry = AuthorityExitRetry::default();
            loop {
                let (next_anchor, listen_for_changes) = {
                    let Some(control) = handler.upgrade() else {
                        break;
                    };
                    let Some(owner) = &control.config_runtime else {
                        break;
                    };
                    let Ok((next, withdrawals, exits, pending, recoveries, holds, mut releases)) =
                        owner
                            .on_blocking(|owner| {
                                let _ = owner.check_recovery();
                                let next = owner.retry_anchor().ok().flatten();
                                let withdrawals = owner.take_phone_withdrawals();
                                let exits = owner.delivery_exits().unwrap_or_default();
                                let pending = owner
                                    .pending_config_restarts()
                                    .unwrap_or_default()
                                    .into_iter()
                                    .filter(|(module, _)| {
                                        owner.delivery_exit_fence(module).ok().flatten().is_none()
                                    })
                                    .collect::<Vec<_>>();
                                let recoveries =
                                    owner.delivery_take_recoveries().unwrap_or_default();
                                let (holds, releases) = owner.take_admission_transitions()?;
                                Ok((
                                    next,
                                    withdrawals,
                                    exits,
                                    pending,
                                    recoveries,
                                    holds,
                                    releases,
                                ))
                            })
                            .await
                    else {
                        break;
                    };
                    let (ready_exit, mut retained_exit) = owner
                        .on_blocking(|owner| {
                            Ok((
                                owner.pending_authority_exit()?,
                                owner.authority_exit_retry_generation()?,
                            ))
                        })
                        .await
                        .unwrap_or((None, exit_retry.generation));
                    if let Some(generation) = ready_exit {
                        if let Err(error) = control
                            .reconcile_authority_exit(owner.clone(), generation)
                            .await
                        {
                            warn!(%error, "approved configuration exit could not be reconciled");
                        } else {
                            retained_exit = None;
                        }
                    }
                    exit_retry.observe_pending(retained_exit, Instant::now());
                    if owner.is_recovering() {
                        for module in control.supervisor.list() {
                            if recovery_holds.insert(module.module_id().to_owned()) {
                                tasks.spawn(async move {
                                    let _ = module.config_exit_hold().await;
                                });
                            }
                        }
                    } else {
                        releases.extend(recovery_holds.drain());
                    }
                    for module in holds {
                        if let Some(supervised) = control
                            .supervisor
                            .get(&module)
                            .filter(|_| owner.check_module_hold(&module).is_err())
                        {
                            tasks.spawn(async move {
                                let _ = supervised.config_exit_hold().await;
                            });
                        }
                    }
                    for module in releases.into_iter().collect::<HashSet<_>>() {
                        if owner.check_module_hold(&module).is_err() {
                            continue;
                        }
                        if let Some(supervised) =
                            control.supervisor.get(&module).filter(|supervised| {
                                supervised
                                    .status()
                                    .is_ok_and(|status| status.enabled && !status.process_alive)
                            })
                        {
                            tasks.spawn(async move {
                                let _ = supervised.set_enabled(true).await;
                            });
                        }
                    }
                    for withdrawal in withdrawals {
                        let owner = Arc::downgrade(owner);
                        tasks.spawn(async move {
                            let _ = withdrawal.sink.send_flushed(withdrawal.frame).await;
                            if let Some(owner) = owner.upgrade() {
                                owner.phone_withdrawn(withdrawal.connection);
                            }
                        });
                    }
                    for exited in exits {
                        let module = exited.module.clone();
                        if recording
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .insert(module.clone())
                        {
                            let handler = handler.clone();
                            let recording = recording.clone();
                            tasks.spawn(async move {
                                if record_exit(handler, exited).await {
                                    recording
                                        .lock()
                                        .unwrap_or_else(|p| p.into_inner())
                                        .remove(&module);
                                }
                            });
                        }
                    }
                    {
                        restarting.retain(|entry| pending.contains(entry));
                        for (module, generation) in pending {
                            if let Some(supervised) = control.supervisor.get(&module) {
                                if restarting.insert((module.clone(), generation)) {
                                    tasks.spawn(async move {
                                        if let Err(error) = supervised.config_apply_restart(generation).await {
                                            warn!(module_id=%module, %error, "approved configuration restart could not be queued");
                                        }
                                    });
                                }
                            }
                        }
                    }
                    let registrations = control
                        .registry
                        .list_registered_modules()
                        .unwrap_or_default();
                    wakes.retain(|connection, wake| {
                        let current = registrations
                            .iter()
                            .any(|registration| registration.connection_id == *connection);
                        if !current {
                            wake.notify_one();
                        }
                        current
                    });
                    for registration in registrations.into_iter().filter(|registration| {
                        registration
                            .control_ops
                            .iter()
                            .any(|op| op == "config.review")
                    }) {
                        let connection = registration.connection_id;
                        if let Some(wake) = wakes.get(&connection) {
                            wake.notify_one();
                        } else {
                            let wake = Arc::new(Notify::new());
                            wakes.insert(connection, wake.clone());
                            tasks.spawn(module_worker(handler.clone(), registration, wake));
                        }
                    }
                    for (connection, module, disposition) in recoveries {
                        if matches!(
                            disposition,
                            AppliedDisposition::Failed {
                                keep_running: true,
                                ..
                            }
                        ) {
                            continue;
                        }
                        if let Some(supervised) = control.supervisor.get(&module) {
                            let hold = matches!(
                                disposition,
                                AppliedDisposition::Failed { restore: None, .. }
                            );
                            tasks.spawn(async move {
                                // Restarting after a rejected configuration
                                // spends one crash-budget entry: the module
                                // failed. An approved restart spends none.
                                if let Err(error) = supervised.config_delivery_failed(connection, hold).await {
                                    warn!(module_id=%module, %error, "configuration delivery recovery request failed");
                                }
                            });
                        }
                    }
                    while let Some(result) = tasks.try_join_next() {
                        if let Err(error) = result {
                            warn!(%error, "configuration delivery task stopped");
                        }
                    }
                    #[cfg(test)]
                    let listen_for_changes =
                        !(retained_exit.is_some() && control.recovery_exit_test_quiet_retry);
                    #[cfg(not(test))]
                    let listen_for_changes = true;
                    let next = next.into_iter().chain(exit_retry.next).min();
                    (next, listen_for_changes)
                };
                tokio::select! {
                    _ = changed.notified(), if listen_for_changes => {},
                    _ = async { match next_anchor { Some(at) => sleep_until(at).await, None => std::future::pending().await } } => {},
                }
            }
        }))
    }
}

#[cfg(test)]
#[tokio::test(start_paused = true)]
async fn config_recovery_exit_retry_backoff_caps_and_does_not_postpone_on_owner_wakes() {
    let start = Instant::now();
    let mut retry = AuthorityExitRetry::default();
    retry.observe_pending(Some(7), start);
    for seconds in [1, 3, 7, 15, 31, 63, 123, 183] {
        let deadline = start + Duration::from_secs(seconds);
        assert_eq!(retry.next, Some(deadline));
        retry.observe_pending(Some(7), deadline - Duration::from_nanos(1));
        assert_eq!(
            retry.next,
            Some(deadline),
            "owner wakes must not defer a retry"
        );
        retry.observe_pending(Some(7), deadline);
    }
    assert_eq!(retry.next, Some(start + Duration::from_secs(243)));
    retry.observe_pending(None, start + Duration::from_secs(184));
    assert!(retry.next.is_none());
    retry.observe_pending(Some(8), start + Duration::from_secs(184));
    assert_eq!(retry.next, Some(start + Duration::from_secs(185)));
}

async fn module_worker(
    handler: std::sync::Weak<ControlHandler>,
    registration: crate::registry::ModuleRegistration,
    wake: Arc<Notify>,
) {
    loop {
        let next = {
            let Some(control) = handler.upgrade() else {
                break;
            };
            let Some(owner) = &control.config_runtime else {
                break;
            };
            let module = &registration.manifest.module_id;
            let Ok(gate) = owner.module_gate(module) else {
                break;
            };
            let mut _guard = gate.lock().await;
            if module_work(owner, module, |owner, module| {
                owner.delivery_exit_fence(&module)
            })
            .await
            .ok()
            .flatten()
            .is_some()
            {
                drop(_guard);
                drop(control);
                wake.notified().await;
                continue;
            }
            let active = control
                .registry
                .get_module(module)
                .ok()
                .flatten()
                .is_some_and(|target| target.connection_id == registration.connection_id);
            let Some(sink) = control
                .forwarding
                .config_delivery_sink(module, registration.connection_id)
                .ok()
                .flatten()
                .filter(|_| active)
            else {
                break;
            };
            let connection = registration.connection_id;
            if owner
                .on_blocking(move |owner| owner.take_unstamp_hint(connection))
                .await
                .unwrap_or(false)
            {
                let hint = subc_protocol::config::ConfigChanged::new(0, vec![]);
                if let Ok(frame) = serde_json::to_vec(&hint)
                    .ok()
                    .and_then(|body| {
                        Frame::build_with_version(
                            registration.negotiated_ver,
                            FrameType::Push,
                            control_flags(),
                            0,
                            0,
                            0,
                            body,
                        )
                        .ok()
                    })
                    .ok_or(())
                {
                    drop(_guard);
                    let _ = timeout_at(
                        Instant::now() + CONFIG_LIVE_ACK_TIMEOUT,
                        sink.send_flushed(frame),
                    )
                    .await;
                    _guard = gate.lock().await;
                }
            }
            let view = module_work(owner, module, move |owner, module| {
                owner.module_get(
                    connection,
                    &module,
                    subc_protocol::config::ConfigGetRequest::new(),
                )
            })
            .await;
            if let Ok(view) = view {
                if let Some(pending) = view.pending {
                    if let Some(attempt) = pending.attempt {
                        let generation = pending.generation;
                        if module_work(owner, module, move |owner, module| {
                            owner.route_policy_needed(&module, generation)
                        })
                        .await
                        .unwrap_or(true)
                        {
                            if let Err(error) = owner
                                .install_approved_route_policy(module, pending.generation)
                                .await
                            {
                                warn!(module_id=%module, %error, "approved route policy could not be installed");
                                drop(_guard);
                                drop(control);
                                wake.notified().await;
                                continue;
                            }
                        }
                        if let Ok(Some((generation, attempt))) = owner
                            .on_blocking(move |owner| owner.policy_probe_due(connection))
                            .await
                        {
                            let supervised = control.supervisor.get(module);
                            drop(_guard);
                            drop(control);
                            if let Some(supervised) = supervised {
                                if let Err(error) = supervised
                                    .config_policy_probe(
                                        registration.connection_id,
                                        generation,
                                        attempt,
                                    )
                                    .await
                                {
                                    warn!(%error, "post-policy configuration health probe could not be queued");
                                }
                            }
                            continue;
                        }
                        let window = owner
                            .on_blocking(move |owner| owner.delivery_window(connection))
                            .await
                            .ok()
                            .flatten();
                        if window.is_some_and(|window| !window.delivered) {
                            let hint = subc_protocol::config::ConfigChanged::new(
                                pending.generation,
                                vec![],
                            );
                            let frame = serde_json::to_vec(&hint).ok().and_then(|body| {
                                Frame::build_with_version(
                                    registration.negotiated_ver,
                                    FrameType::Push,
                                    control_flags(),
                                    0,
                                    0,
                                    0,
                                    body,
                                )
                                .ok()
                            });
                            if let Some(frame) = frame {
                                let _ = owner
                                    .on_blocking(move |owner| {
                                        owner.delivery_write_started(
                                            connection,
                                            generation,
                                            attempt,
                                            Instant::now(),
                                        )
                                    })
                                    .await;
                                let flush_deadline = window
                                    .and_then(|window| window.deadline)
                                    .map_or(Instant::now() + CONFIG_LIVE_ACK_TIMEOUT, |at| {
                                        at.min(Instant::now() + CONFIG_LIVE_ACK_TIMEOUT)
                                    });
                                drop(_guard);
                                let flushed =
                                    timeout_at(flush_deadline, sink.send_flushed(frame)).await;
                                let flushed_at = Instant::now();
                                _guard = gate.lock().await;
                                let current = control
                                    .config_delivery_target_current(
                                        module,
                                        registration.connection_id,
                                        true,
                                        0,
                                    )
                                    .unwrap_or(false)
                                    && owner
                                        .on_blocking(move |owner| owner.delivery_window(connection))
                                        .await
                                        .ok()
                                        .flatten()
                                        .is_some_and(|window| {
                                            (window.generation, window.attempt)
                                                == (pending.generation, attempt)
                                        });
                                if !current {
                                    continue;
                                }
                                match flushed {
                                    Ok(Ok(())) => {
                                        if owner
                                            .on_blocking(move |owner| {
                                                owner.delivery_written(
                                                    connection, generation, attempt, flushed_at,
                                                )
                                            })
                                            .await
                                            .is_err()
                                        {
                                            let _ =
                                                module_work(owner, module, move |owner, module| {
                                                    owner.delivery_timeout(
                                                        connection, &module, generation, attempt,
                                                    )
                                                })
                                                .await;
                                        }
                                    }
                                    _ => {
                                        let _ = module_work(owner, module, move |owner, module| {
                                            owner.delivery_timeout(
                                                connection, &module, generation, attempt,
                                            )
                                        })
                                        .await;
                                    }
                                }
                                owner.notify_delivery_changed();
                            }
                        }
                    }
                }
            }
            let window = owner
                .on_blocking(move |owner| owner.delivery_window(connection))
                .await
                .ok()
                .flatten();
            match window {
                Some(window) if window.deadline.is_some_and(|at| Instant::now() >= at) => {
                    let _ = module_work(owner, module, move |owner, module| {
                        owner.delivery_timeout(
                            connection,
                            &module,
                            window.generation,
                            window.attempt,
                        )
                    })
                    .await;
                    owner.notify_delivery_changed();
                    None
                }
                Some(window) => window.deadline,
                None => None,
            }
        };
        tokio::select! {
            _ = wake.notified() => {},
            _ = async { match next { Some(at) => sleep_until(at).await, None => std::future::pending().await } } => {},
        }
    }
}

async fn record_exit(
    handler: std::sync::Weak<ControlHandler>,
    exited: crate::config_gate::runtime::delivery::ExitedDelivery,
) -> bool {
    let Some(control) = handler.upgrade() else {
        return true;
    };
    let Some(owner) = &control.config_runtime else {
        return true;
    };
    let mut failure = None;
    // Reconciliation is idempotent. Three attempts, with 50/100 ms backoff,
    // cover transient record/hold errors without an endless hidden retry loop.
    for delay in [
        Duration::ZERO,
        Duration::from_millis(50),
        Duration::from_millis(100),
    ] {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        match reconcile_exit(&control, owner, &exited).await {
            Ok(()) => return true,
            Err(reason) => {
                failure.get_or_insert(reason);
            }
        }
    }
    let reason = failure.expect("each unsuccessful attempt records a reason");
    if let Some(module) = control.supervisor.get(&exited.module) {
        // The exit fence still prevents respawn. Never wait for this command
        // under the module gate: an earlier restart may need that same gate.
        if reason.code != "config_exit_hold_failed" {
            let _ = hold_exited_module(&module).await;
        }
        if let Err(error) = module.config_exit_failed(reason.clone()) {
            warn!(module_id=%exited.module, %error, "configuration exit stopped-state diagnostic could not be retained");
        }
    }
    if let Some(ledger) = &control.activity {
        ledger.config_exit_failed(&exited.module, &reason);
    }
    warn!(module_id=%exited.module, reason=%reason.code, error_kind=%reason.error_kind,
        "configuration exit reconciliation exhausted its retries; module remains stopped behind its exit fence");
    // Retain the coordinator's per-module entry. Future notifications must not
    // silently restart this bounded retry cycle while the terminal fence stays.
    false
}

async fn reconcile_exit(
    control: &ControlHandler,
    owner: &Arc<crate::config_gate::runtime::RuntimeOwner>,
    exited: &crate::config_gate::runtime::delivery::ExitedDelivery,
) -> Result<(), crate::supervise::ModuleStopReason> {
    let reason = |code: &str, error_kind: String| crate::supervise::ModuleStopReason {
        code: code.into(),
        error_kind,
    };
    let gate = owner
        .module_gate(&exited.module)
        .map_err(|error| reason("config_exit_record_failed", error.code))?;
    let hold = {
        let _guard = gate.lock().await;
        let exited = exited.clone();
        owner
            .on_blocking(move |owner| owner.delivery_record_exit(&exited))
            .await
            .map_err(|error| reason("config_exit_record_failed", error.code))?
    };
    if hold {
        if let Some(module) = control.supervisor.get(&exited.module) {
            hold_exited_module(&module)
                .await
                .map_err(|kind| reason("config_exit_hold_failed", kind.into()))?;
        }
    }
    let _guard = gate.lock().await;
    let exited = exited.clone();
    owner
        .on_blocking(move |owner| owner.delivery_release_exit(&exited))
        .await
        .map_err(|error| reason("config_exit_release_failed", error.code))
}

// A stuck supervisor cannot leave reconciliation parked forever. A timed-out
// hold may still finish later; it can only stop this already-exited child.
const CONFIG_EXIT_HOLD_TIMEOUT: Duration = Duration::from_secs(5);

async fn hold_exited_module(
    module: &crate::supervise::SupervisedModule,
) -> Result<(), &'static str> {
    match tokio::time::timeout(CONFIG_EXIT_HOLD_TIMEOUT, module.config_exit_hold()).await {
        Ok(Ok(())) => Ok(()),
        Err(_) => Err("supervisor_hold_timeout"),
        Ok(Err(crate::supervise::SuperviseError::CommandClosed { .. })) => {
            Err("supervisor_command_closed")
        }
        Ok(Err(crate::supervise::SuperviseError::StatePoisoned { .. })) => {
            Err("supervisor_state_poisoned")
        }
        Ok(Err(_)) => Err("supervisor_error"),
    }
}

async fn module_work<T: Send + 'static>(
    owner: &Arc<crate::config_gate::runtime::RuntimeOwner>,
    module: &str,
    work: impl FnOnce(Arc<crate::config_gate::runtime::RuntimeOwner>, String) -> Result<T, GateError>
        + Send
        + 'static,
) -> Result<T, GateError> {
    let module = module.to_owned();
    owner.on_blocking(move |owner| work(owner, module)).await
}
