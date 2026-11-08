//! Owned tasks isolate each module's outbound delivery and acknowledgment deadlines.

use super::*;
use crate::config_gate::runtime::{
    delivery::CONFIG_LIVE_ACK_TIMEOUT, AppliedDisposition, DeliveryObserver,
};
use std::collections::{HashMap, HashSet};
use tokio::sync::Notify;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{sleep_until, timeout_at, Instant};

impl ControlHandler {
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
            loop {
                let next_anchor = {
                    let Some(control) = handler.upgrade() else {
                        break;
                    };
                    let Some(owner) = &control.config_runtime else {
                        break;
                    };
                    let next = owner.retry_anchor().ok().flatten();
                    for withdrawal in owner.take_phone_withdrawals() {
                        let owner = Arc::downgrade(owner);
                        tasks.spawn(async move {
                            let _ = withdrawal.sink.send_flushed(withdrawal.frame).await;
                            if let Some(owner) = owner.upgrade() {
                                owner.phone_withdrawn(withdrawal.connection);
                            }
                        });
                    }
                    for exited in owner.delivery_exits().unwrap_or_default() {
                        let module = exited.module.clone();
                        if recording
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .insert(module.clone())
                        {
                            let handler = handler.clone();
                            let recording = recording.clone();
                            tasks.spawn(async move {
                                record_exit(handler, exited).await;
                                recording
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner())
                                    .remove(&module);
                            });
                        }
                    }
                    if let Ok(pending) = owner.pending_config_restarts() {
                        restarting.retain(|entry| pending.contains(entry));
                        for (module, generation) in pending {
                            if owner.delivery_exit_fence(&module).ok().flatten().is_some() {
                                continue;
                            }
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
                    for (connection, module, disposition) in
                        owner.delivery_take_recoveries().unwrap_or_default()
                    {
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
                    next
                };
                tokio::select! {
                    _ = changed.notified() => {},
                    _ = async { match next_anchor { Some(at) => sleep_until(at).await, None => std::future::pending().await } } => {},
                }
            }
        }))
    }
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
            let _guard = gate.lock().await;
            if owner.delivery_exit_fence(module).ok().flatten().is_some() {
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
            let view = owner.module_get(
                registration.connection_id,
                module,
                subc_protocol::config::ConfigGetRequest::new(),
            );
            if let Ok(view) = view {
                if let Some(pending) = view.pending {
                    if let Some(attempt) = pending.attempt {
                        if owner
                            .route_policy_needed(module, pending.generation)
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
                        if let Ok(Some((generation, attempt))) =
                            owner.policy_probe_due(registration.connection_id)
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
                            .delivery_window(registration.connection_id)
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
                                let _ = owner.delivery_write_started(
                                    registration.connection_id,
                                    pending.generation,
                                    attempt,
                                    Instant::now(),
                                );
                                let flush_deadline = window
                                    .and_then(|window| window.deadline)
                                    .map_or(Instant::now() + CONFIG_LIVE_ACK_TIMEOUT, |at| {
                                        at.min(Instant::now() + CONFIG_LIVE_ACK_TIMEOUT)
                                    });
                                match timeout_at(flush_deadline, sink.send_flushed(frame)).await {
                                    Ok(Ok(())) => {
                                        if owner
                                            .delivery_written(
                                                registration.connection_id,
                                                pending.generation,
                                                attempt,
                                                Instant::now(),
                                            )
                                            .is_err()
                                        {
                                            let _ = owner.delivery_timeout(
                                                registration.connection_id,
                                                module,
                                                pending.generation,
                                                attempt,
                                            );
                                        }
                                    }
                                    _ => {
                                        let _ = owner.delivery_timeout(
                                            registration.connection_id,
                                            module,
                                            pending.generation,
                                            attempt,
                                        );
                                    }
                                }
                                owner.notify_delivery_changed();
                            }
                        }
                    }
                }
            }
            let window = owner
                .delivery_window(registration.connection_id)
                .ok()
                .flatten();
            match window {
                Some(window) if window.deadline.is_some_and(|at| Instant::now() >= at) => {
                    let _ = owner.delivery_timeout(
                        registration.connection_id,
                        module,
                        window.generation,
                        window.attempt,
                    );
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
) {
    let Some(control) = handler.upgrade() else {
        return;
    };
    let Some(owner) = &control.config_runtime else {
        return;
    };
    let Ok(gate) = owner.module_gate(&exited.module) else {
        return;
    };
    let _guard = gate.lock().await;
    match owner.delivery_record_exit(&exited) {
        Ok(hold) => {
            if hold {
                if let Some(module) = control.supervisor.get(&exited.module) {
                    if let Err(error) = module.config_exit_hold().await {
                        warn!(module_id=%exited.module, %error, "configuration exit hold could not be recorded");
                        return;
                    }
                }
            }
            if let Err(error) = owner.delivery_release_exit(&exited) {
                warn!(module_id=%exited.module, %error, "configuration exit fence could not be released");
            }
        }
        Err(error) => {
            warn!(module_id=%exited.module, %error, "configuration exit failure could not be recorded")
        }
    }
}
