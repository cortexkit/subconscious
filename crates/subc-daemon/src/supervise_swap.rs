//! Blue/green swap of one supervised module.
//!
//! A swap starts a CANDIDATE process beside the running INCUMBENT, routes
//! nothing to it while it warms, and once it has declared itself ready moves new
//! routes onto it and drains the incumbent. The sequence, and why each step is
//! shaped the way it is, is `docs/designs/module-readiness-and-swap.md`
//! ("Rung 3").
//!
//! Two rules run through everything here:
//!
//! * Until cutover the incumbent is the module. Nothing on a failure path may
//!   drain it, touch its snapshot, move its nonce, or spend a unit of its
//!   crash-restart budget. A candidate that fails is killed, its slot is freed,
//!   and the swap reports which arm failed; the module is exactly as it was.
//! * The candidate is reached only by connection or by endpoint, never by
//!   module id, because every by-id lookup resolves the incumbent.
//!
//! The whole swap runs inside the module's supervise loop, like a restart, and
//! the incumbent is not health-probed while it runs. But the candidate's warm-up
//! can take the whole readiness budget, so it keeps serving the module's
//! commands meanwhile (see [`serve_command_while_warming`]): an operator stop,
//! disable or retire aborts the swap and is then carried out on the incumbent,
//! instead of queueing for up to the budget. The operator's reply is sent at
//! cutover or failure, before the incumbent's drain, so a caller whose own
//! requests ride the incumbent is not left waiting on a drain that is waiting
//! on it.

use super::*;
use crate::registry::RegistrationSlot;

/// The cgroup directory name for one process of `module_id`.
///
/// A swap overlaps two processes of one module and they must not share a
/// cgroup (it would make them one kill domain), so each module has two names
/// and a swap's candidate takes whichever the incumbent is not using, keeping
/// it after cutover. The alternate name is the primary one plus `_swap`.
///
/// The encoding is injective, so no module id can name another module's
/// cgroup, swap or not. Bytes outside `[A-Za-z0-9.-]`, the underscore
/// included, are written as `_` and two lowercase hex digits; so every `_`
/// the encoding produces is followed by two hex digits, and `_swap` (with `s`
/// not a hex digit) can only be the suffix. The cgroup library's own escaping
/// leaves this alphabet untouched. Before swap existed the library's encoding
/// was used directly, and it passes `_` through, so `a_40swap` and `a@swap`
/// (and `a` + `_40swap`) named one directory; for ids containing `_` the
/// primary name therefore differs from what earlier daemons created.
pub(crate) fn cgroup_name(module_id: &str, alternate: bool) -> String {
    let mut name = String::with_capacity(module_id.len() + 5);
    for byte in module_id.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.') {
            name.push(char::from(byte));
        } else {
            name.push_str(&format!("_{byte:02x}"));
        }
    }
    if alternate {
        name.push_str("_swap");
    }
    name
}

/// How a candidate's warm-up ended.
enum Warm {
    /// Registered and ready (or registered while the incumbent was gone), on
    /// this connection.
    Ready(ConnectionId),
    Failed(CandidateFailure),
    /// An operator command that must win over the swap arrived while the
    /// candidate warmed. `connection` is the candidate's, if it registered.
    Interrupted {
        command: Option<SupervisorCommand>,
        connection: Option<ConnectionId>,
    },
}

/// What a swap hands back to the supervise loop when it ends.
#[derive(Default)]
pub(super) struct SwapEnd {
    /// Commands for the loop to run next, in arrival order: configuration
    /// updates that arrived during the warm-up (already answered, applied now
    /// that the swap no longer holds the spec), then the stop, disable or
    /// retire that interrupted the swap, if one did.
    pub(super) requeue: Vec<SupervisorCommand>,
}

struct CandidateFailure {
    arm: SwapFailureArm,
    detail: String,
    /// Set when the candidate exited on its own; it is then already reaped.
    exit: Option<ExitReport>,
    /// The candidate's connection, when it registered.
    connection: Option<ConnectionId>,
}

impl CandidateFailure {
    fn into_error(self, module_id: &str) -> SuperviseError {
        SuperviseError::SwapFailed {
            module_id: module_id.to_string(),
            arm: self.arm,
            detail: self.detail,
            candidate_exit: self.exit,
        }
    }
}

/// Run one swap to completion. Replies on `reply` at cutover or failure.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_swap(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<SupervisedChild>,
    commands: &mut mpsc::Receiver<SupervisorCommand>,
    ready_timeout: Duration,
    reply: oneshot::Sender<Result<(), SuperviseError>>,
) -> SwapEnd {
    let mut end = SwapEnd::default();
    run_swap_inner(
        spec,
        runtime,
        registry,
        process_liveness,
        snapshot,
        child,
        commands,
        ready_timeout,
        reply,
        &mut end,
    )
    .await;
    end
}

#[allow(clippy::too_many_arguments)]
async fn run_swap_inner(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<SupervisedChild>,
    commands: &mut mpsc::Receiver<SupervisorCommand>,
    ready_timeout: Duration,
    reply: oneshot::Sender<Result<(), SuperviseError>>,
    end: &mut SwapEnd,
) {
    let module_id = spec.module_id.as_str();
    let (forwarding, handle, incumbent_connection) =
        match admit_swap(spec, runtime, registry, snapshot, child) {
            Ok(admitted) => admitted,
            Err(err) => {
                info!(module_id, error = %err, "swap refused before spawning a candidate");
                let _ = reply.send(Err(err));
                return;
            }
        };

    // The candidate takes whichever cgroup the incumbent is not using.
    let candidate_alternate = !lock_snapshot(snapshot)
        .map(|state| state.in_alternate_slot)
        .unwrap_or(false);
    let mut candidate = match spawn_child_in_slot(
        spec,
        runtime.connection_file_path.as_deref(),
        Some(&handle),
        &runtime.stderr_ring,
        runtime.capture_logs_dir.as_deref(),
        &runtime.child_roster,
        #[cfg(target_os = "linux")]
        runtime.cgroup_placement.as_ref(),
        SpawnRole::SwapCandidate,
        candidate_alternate,
    ) {
        Ok(candidate) => candidate,
        Err(err) => {
            handle.close_swap(module_id);
            warn!(module_id, error = %err, "swap candidate failed to spawn; incumbent untouched");
            let _ = reply.send(Err(SuperviseError::SwapFailed {
                module_id: module_id.to_string(),
                arm: SwapFailureArm::SpawnFailed,
                detail: err.to_string(),
                candidate_exit: None,
            }));
            return;
        }
    };
    info!(
        module_id,
        candidate_pid = candidate.pid,
        cgroup = %cgroup_name(module_id, candidate_alternate),
        incumbent_connection_id = incumbent_connection.get(),
        ready_timeout_ms = ready_timeout.as_millis() as u64,
        "swap candidate spawned; routing stays on the incumbent until it is ready"
    );

    let warm = warm_candidate(
        module_id,
        registry,
        &mut candidate,
        incumbent_connection,
        ready_timeout,
        commands,
        end,
    )
    .await;
    let candidate_connection = match warm {
        Warm::Ready(connection) => connection,
        Warm::Interrupted {
            command,
            connection,
        } => {
            let failure = CandidateFailure {
                arm: SwapFailureArm::Interrupted,
                detail: "an operator stop, disable or retire arrived while the candidate warmed"
                    .to_string(),
                exit: None,
                connection,
            };
            abandon_candidate(
                module_id,
                registry,
                &forwarding,
                &handle,
                candidate,
                &failure,
            )
            .await;
            let _ = reply.send(Err(failure.into_error(module_id)));
            end.requeue.extend(command);
            return;
        }
        Warm::Failed(failure) => {
            abandon_candidate(
                module_id,
                registry,
                &forwarding,
                &handle,
                candidate,
                &failure,
            )
            .await;
            let _ = reply.send(Err(failure.into_error(module_id)));
            return;
        }
    };

    if let Err(failure) = probe_candidate(
        module_id,
        runtime,
        registry,
        &forwarding,
        candidate_connection,
    )
    .await
    {
        abandon_candidate(
            module_id,
            registry,
            &forwarding,
            &handle,
            candidate,
            &failure,
        )
        .await;
        let _ = reply.send(Err(failure.into_error(module_id)));
        return;
    }

    // CUTOVER. Forwarding first, then the registry: two calls under two locks,
    // so there is a moment when they disagree, and the order decides which way.
    // Every route.open reads the registry and then reserves its relay under the
    // forwarding write lock, and the reservation alone picks the process. With
    // forwarding first, a route.open in the gap reads the incumbent's
    // registration and is relayed to the candidate, which is ready (that is
    // what was just waited for) and is the process the module now is. Nothing
    // can land on a process that is not serving: before the forwarding write
    // it lands on the incumbent, after it on the candidate. And from the moment
    // the registry names the candidate, forwarding already routes to it, so
    // nothing that reads the registry (catalog.list, the health prober) is ever
    // ahead of where routes go.
    let forwarding_cutover = match forwarding.cutover_candidate(module_id) {
        Ok(Some(cutover)) => cutover,
        Ok(None) | Err(_) => {
            // The candidate's forwarding entry is gone, so its connection was
            // torn down just now. Nothing was promoted; the incumbent is still
            // the active endpoint.
            let failure = CandidateFailure {
                arm: SwapFailureArm::CutoverLost,
                detail: "the candidate's connection closed just before cutover".to_string(),
                exit: None,
                connection: Some(candidate_connection),
            };
            abandon_candidate(
                module_id,
                registry,
                &forwarding,
                &handle,
                candidate,
                &failure,
            )
            .await;
            let _ = reply.send(Err(failure.into_error(module_id)));
            return;
        }
    };
    let promoted = match registry.promote_candidate(module_id) {
        Ok(Some(cutover)) => cutover.promoted,
        Ok(None) | Err(_) => {
            // Forwarding promoted the candidate and the registry could not: its
            // registration went away between the two calls, so the process that
            // was just made the active endpoint is gone and the incumbent sits in
            // forwarding's superseded slot. There is no way back to the incumbent
            // from here, so fall back to a plain restart, which stops the
            // incumbent and spawns a fresh process. It is a restart the operator
            // asked for, so it spends no crash budget either.
            error!(
                module_id,
                "swap candidate vanished between forwarding cutover and registry promotion; falling back to a plain restart"
            );
            let failure = CandidateFailure {
                arm: SwapFailureArm::CutoverLost,
                detail: "the candidate's registration closed during cutover; the module is being restarted plainly".to_string(),
                exit: None,
                connection: Some(candidate_connection),
            };
            abandon_candidate(
                module_id,
                registry,
                &forwarding,
                &handle,
                candidate,
                &failure,
            )
            .await;
            let _ = reply.send(Err(failure.into_error(module_id)));
            if let Err(err) = restart_child(
                spec,
                runtime,
                registry,
                process_liveness,
                snapshot,
                child,
                runtime.drain_timeout,
            )
            .await
            {
                warn!(module_id, error = %err, "plain restart after a lost swap cutover failed");
                fail_snapshot(snapshot, Some(module_id), None);
            }
            return;
        }
    };

    // The candidate is the module now: its nonce is the module's nonce, the
    // control plane's capability state is recomputed from its manifest (its
    // HELLO skipped that, not being routable then), and the snapshot describes
    // its process.
    handle.promote_swap_nonce(module_id, spec.reserved);
    handle.notify_swap_promoted(&promoted);
    let incumbent_generation = lock_snapshot(snapshot)
        .map(|state| state.spawn_generation)
        .unwrap_or(0);
    if let Err(err) = set_running(snapshot, &candidate, module_id, &runtime.spawn_events) {
        error!(module_id, error = %err, "failed to record the promoted swap candidate");
    }
    let _ = update_snapshot(snapshot, Some(module_id), |state| {
        state.in_alternate_slot = candidate_alternate;
    });
    process_liveness.track(module_id.to_string(), Arc::clone(snapshot));
    let incumbent = child.replace(candidate);
    info!(
        module_id,
        promoted_connection_id = candidate_connection.get(),
        superseded_connection_id = incumbent_connection.get(),
        "swap cut over; new routes land on the promoted candidate, draining the incumbent"
    );
    let _ = reply.send(Ok(()));

    retire_incumbent(
        spec,
        runtime,
        registry,
        &forwarding,
        snapshot,
        incumbent,
        forwarding_cutover.incumbent,
        incumbent_connection,
        incumbent_generation,
    )
    .await;
    handle.close_swap(module_id);
}

/// Answer one module command that arrived while the candidate warmed, or hand
/// it back when it must interrupt the swap.
///
/// A stop, disable or retire interrupts: the operator's intent wins over a
/// swap, as it does over a pending crash respawn, and it must not wait out the
/// readiness budget. It is returned so the swap can kill its candidate and the
/// loop can then carry it out on the incumbent. Restart, reload and a second
/// swap are refused with a typed error rather than queued; enabling an already
/// enabled module is answered as the no-op it is. A configuration update is
/// answered at once and applied when the swap ends, since the swap is using the
/// spec it replaces.
fn serve_command_while_warming(
    module_id: &str,
    command: SupervisorCommand,
    end: &mut SwapEnd,
) -> Option<SupervisorCommand> {
    let in_progress = || SuperviseError::SwapInProgress {
        module_id: module_id.to_string(),
    };
    match command {
        SupervisorCommand::Drain { .. }
        | SupervisorCommand::Retire { .. }
        | SupervisorCommand::SetEnabled { enabled: false, .. } => Some(command),
        SupervisorCommand::SetEnabled {
            enabled: true,
            reply,
        } => {
            let _ = reply.send(Ok(false));
            None
        }
        SupervisorCommand::Restart { reply, .. } | SupervisorCommand::Reload { reply } => {
            let _ = reply.send(Err(in_progress()));
            None
        }
        SupervisorCommand::Swap { reply, .. } => {
            let _ = reply.send(Err(SuperviseError::SwapRefused {
                module_id: module_id.to_string(),
                reason: SwapRefusal::AlreadySwapping,
            }));
            None
        }
        SupervisorCommand::UpdateConfiguration {
            spec,
            health,
            drain_timeout_ms,
            reply,
        } => {
            let _ = reply.send(());
            // The caller has its answer; the replayed command's reply channel
            // has no receiver and is only there to fit the command's shape.
            let (unanswered, _) = oneshot::channel();
            end.requeue.push(SupervisorCommand::UpdateConfiguration {
                spec,
                health,
                drain_timeout_ms,
                reply: unanswered,
            });
            None
        }
    }
}

/// Every check a swap makes before spawning anything. Returns the forwarding
/// table, the shared handle, and the incumbent's connection.
fn admit_swap(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    snapshot: &SharedSnapshot,
    child: &Option<SupervisedChild>,
) -> Result<(Arc<ForwardingTable>, SupervisorHandle, ConnectionId), SuperviseError> {
    let module_id = spec.module_id.as_str();
    let refuse = |reason| SuperviseError::SwapRefused {
        module_id: module_id.to_string(),
        reason,
    };
    // First, and unconditionally: a module that has not declared it tolerates
    // a second process of itself is never given one.
    if spec.overlap != ModuleOverlap::Safe {
        return Err(refuse(SwapRefusal::OverlapExclusive));
    }
    if !lock_snapshot(snapshot)?.enabled {
        return Err(SuperviseError::Disabled {
            module_id: module_id.to_string(),
        });
    }
    if spec.protocol == ModuleProtocol::None {
        return Err(refuse(SwapRefusal::ProtocolNone));
    }
    let (Some(forwarding), Some(handle)) = (
        runtime.forwarding.clone(),
        runtime.supervisor_handle.clone(),
    ) else {
        return Err(refuse(SwapRefusal::NotConfigured));
    };
    if handle.swap_open(module_id) {
        return Err(refuse(SwapRefusal::AlreadySwapping));
    }
    let registration = registry
        .get_module(module_id)
        .map_err(SuperviseError::Registry)?;
    let (Some(registration), true) = (registration, child.is_some()) else {
        return Err(refuse(SwapRefusal::NotRegistered));
    };
    Ok((forwarding, handle, registration.connection_id))
}

/// Wait for the candidate to register and declare itself ready, or to fail.
///
/// Readiness is read from the candidate's own registration, which its
/// `catalog.update(ready: true)` updates through its connection (the registry
/// searches every slot by connection for exactly this). Nothing here goes
/// through a by-id lookup, all of which resolve the incumbent.
///
/// The module's commands are served meanwhile; see
/// [`serve_command_while_warming`].
#[allow(clippy::too_many_arguments)]
async fn warm_candidate(
    module_id: &str,
    registry: &Registry,
    candidate: &mut SupervisedChild,
    incumbent_connection: ConnectionId,
    ready_timeout: Duration,
    commands: &mut mpsc::Receiver<SupervisorCommand>,
    end: &mut SwapEnd,
) -> Warm {
    let deadline = Instant::now() + ready_timeout;
    let mut registered: Option<ConnectionId> = None;
    loop {
        // Before the candidate registers, the candidate slot is the only
        // place to look for it; once it has, its connection names it exactly.
        let slot = match registered {
            Some(connection) => RegistrationSlot::Connection(connection),
            None => RegistrationSlot::Candidate(module_id),
        };
        match registry.registration(slot) {
            Ok(Some(registration)) => {
                registered = Some(registration.connection_id);
                if registration.ready {
                    return Warm::Ready(registration.connection_id);
                }
                // The incumbent died while the candidate warmed. The candidate
                // is now the only process, so waiting for it to finish warming
                // serves nobody: promote it, and callers get `module_warming`
                // until it declares itself ready, as on a plain restart.
                if matches!(
                    registry.registration(RegistrationSlot::Connection(incumbent_connection)),
                    Ok(None)
                ) {
                    warn!(
                        module_id,
                        "incumbent went away while the swap candidate warmed; promoting the candidate before it is ready"
                    );
                    return Warm::Ready(registration.connection_id);
                }
            }
            Ok(None) if registered.is_some() => {
                // It registered and then its registration went away: its
                // connection closed. Its exit is reaped below or next poll.
            }
            Ok(None) => {}
            Err(err) => {
                return Warm::Failed(CandidateFailure {
                    arm: if registered.is_some() {
                        SwapFailureArm::NeverReady
                    } else {
                        SwapFailureArm::NeverRegistered
                    },
                    detail: format!("could not read the candidate's registration: {err}"),
                    exit: None,
                    connection: registered,
                });
            }
        }

        let now = Instant::now();
        if now >= deadline {
            let (arm, detail) = match registered {
                Some(_) => (
                    SwapFailureArm::NeverReady,
                    format!("the candidate registered but did not declare itself ready within {ready_timeout:?}"),
                ),
                None => (
                    SwapFailureArm::NeverRegistered,
                    format!("the candidate did not register within {ready_timeout:?}"),
                ),
            };
            return Warm::Failed(CandidateFailure {
                arm,
                detail,
                exit: None,
                connection: registered,
            });
        }
        let poll = deadline
            .saturating_duration_since(now)
            .min(REGISTRY_RELEASE_POLL);
        tokio::select! {
            status = candidate.wait() => {
                // Classified on its own: the module snapshot's severance marker
                // belongs to the incumbent and must not be consumed here.
                let exit = match status {
                    Ok(status) => classify_exit(&status),
                    Err(_) => wait_error_exit_report(),
                };
                return Warm::Failed(CandidateFailure {
                    arm: SwapFailureArm::CandidateExited,
                    detail: format!(
                        "the candidate exited before it was ready (code {:?}, signal {:?})",
                        exit.code, exit.signal
                    ),
                    exit: Some(exit),
                    connection: registered,
                });
            }
            command = commands.recv() => {
                // A closed channel means the module handle is gone; the loop
                // will see the same and stop, so abandon the candidate first.
                let Some(command) = command else {
                    return Warm::Interrupted { command: None, connection: registered };
                };
                if let Some(command) = serve_command_while_warming(module_id, command, end) {
                    return Warm::Interrupted { command: Some(command), connection: registered };
                }
            }
            _ = sleep(poll) => {}
        }
    }
}

/// Probe a ready candidate once, by endpoint, before promoting it.
///
/// The same `health.check` the supervisor probes every module with, sent only
/// when the candidate advertises it. A `failing` answer or no usable answer
/// fails the swap; `degraded` does not, since a degraded module still serves.
/// Like every other swap failure it spends no crash-restart budget: the
/// budget is the incumbent's, and the incumbent did nothing.
async fn probe_candidate(
    module_id: &str,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    forwarding: &ForwardingTable,
    candidate_connection: ConnectionId,
) -> Result<(), CandidateFailure> {
    let unhealthy = |detail: String| CandidateFailure {
        arm: SwapFailureArm::CandidateUnhealthy,
        detail,
        exit: None,
        connection: Some(candidate_connection),
    };
    let advertises_health = registry
        .registration(RegistrationSlot::Connection(candidate_connection))
        .ok()
        .flatten()
        .is_some_and(|registration| {
            registration
                .control_ops
                .iter()
                .any(|op| op == MODULE_CONTROL_OP_HEALTH_CHECK)
        });
    if !advertises_health {
        return Ok(());
    }
    let Some(endpoint) = forwarding
        .module_endpoint_for_connection(candidate_connection)
        .ok()
        .flatten()
    else {
        return Err(unhealthy(
            "the candidate's connection closed before its health probe".to_string(),
        ));
    };
    match probe_endpoint_health(endpoint, runtime, None).await {
        Ok(report) if report.status == HealthStatus::Failing => Err(unhealthy(format!(
            "the candidate answered its health probe with status failing{}",
            report
                .detail
                .map(|detail| format!(": {detail}"))
                .unwrap_or_default()
        ))),
        Ok(_) => Ok(()),
        Err(err) => {
            debug!(module_id, error = %err, "swap candidate health probe failed");
            Err(unhealthy(format!(
                "the candidate's health probe failed: {err}"
            )))
        }
    }
}

/// Kill a failed candidate and free exactly its slot.
///
/// This is the candidate's own reap path. The ordinary one classifies an exit
/// into the module's snapshot and then asks the crash budget for a respawn;
/// either would be wrong here, because the snapshot and the budget are the
/// incumbent's and the incumbent is still serving. So this kills and reaps
/// only the candidate, waits only for the candidate's registration (by its
/// connection, or by the candidate slot if it never registered) to go, and
/// closes the swap, which releases only the candidate's nonce.
async fn abandon_candidate(
    module_id: &str,
    registry: &Registry,
    forwarding: &ForwardingTable,
    handle: &SupervisorHandle,
    mut candidate: SupervisedChild,
    failure: &CandidateFailure,
) {
    warn!(
        module_id,
        arm = failure.arm.as_str(),
        detail = %failure.detail,
        candidate_pid = candidate.pid,
        "swap failed; killing the candidate and leaving the incumbent serving"
    );
    if failure.exit.is_none() {
        if let Err(err) = candidate.start_kill() {
            debug!(module_id, error = %err, "swap candidate kill failed; it may already have exited");
        }
        if let Err(err) = candidate.wait().await {
            warn!(module_id, error = %err, "could not reap the abandoned swap candidate");
        }
    }
    candidate.drain_stderr(module_id).await;

    let slot = match failure.connection {
        Some(connection) => RegistrationSlot::Connection(connection),
        None => RegistrationSlot::Candidate(module_id),
    };
    if let Err(err) =
        wait_for_slot_registration_release(registry, slot, REGISTRY_RELEASE_TIMEOUT).await
    {
        // The process is dead, so its connection is on its way down; if the
        // registration outlived the wait, close the connection outright so the
        // candidate slot is free for the next swap.
        warn!(module_id, error = %err, "abandoned swap candidate is still registered; closing its connection");
        if let Some(connection) = failure.connection.or_else(|| {
            registry
                .get_candidate(module_id)
                .ok()
                .flatten()
                .map(|registration| registration.connection_id)
        }) {
            forwarding.request_connection_close(
                connection,
                CloseReason::new(
                    "swap_candidate_abandoned",
                    format!("swap of module '{module_id}' failed; closing its candidate"),
                ),
            );
        }
    }
    handle.close_swap(module_id);
}

/// Drain and reap the incumbent a swap has just replaced.
///
/// The drain is the plain restart's drain (route.closing, quiescence,
/// route.closed, per-route GOODBYE, module GOODBYE) with reason `Restart`, so
/// deployed SDKs keep treating it as may-reopen, but addressed to the
/// incumbent's endpoint rather than the module id, which now resolves to the
/// promoted candidate. The incumbent's release is awaited by its connection,
/// since the id's active registration is the candidate's and never goes away.
/// The module's state is not touched: it describes the promoted candidate.
#[allow(clippy::too_many_arguments)]
async fn retire_incumbent(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    forwarding: &ForwardingTable,
    snapshot: &SharedSnapshot,
    incumbent: Option<SupervisedChild>,
    incumbent_endpoint: Option<crate::ModuleEndpointId>,
    incumbent_connection: ConnectionId,
    incumbent_generation: u64,
) {
    let module_id = spec.module_id.as_str();
    if let Some(endpoint) = incumbent_endpoint {
        if let Err(err) = begin_forwarding_drain_with(
            forwarding,
            ForwardingDrainContext {
                spec,
                runtime,
                registry,
                scope: DrainScope::Endpoint(endpoint),
            },
            snapshot,
            None,
            RouteCloseReason::Restart,
            runtime.drain_timeout,
        )
        .await
        {
            warn!(module_id, error = %err, "draining the swapped-out incumbent failed; stopping it anyway");
        }
    }

    let Some(mut incumbent) = incumbent else {
        return;
    };
    // The module GOODBYE above asks the incumbent to exit; give it the same
    // budget a plain restart gives the child it drains, then kill it.
    let status = match timeout(runtime.drain_timeout, incumbent.wait()).await {
        Ok(status) => status,
        Err(_) => {
            warn!(
                module_id,
                pid = incumbent.pid,
                budget_ms = u64::try_from(runtime.drain_timeout.as_millis()).unwrap_or(u64::MAX),
                reason = "swapped-out incumbent",
                "drain budget expired before the module exited; killing it"
            );
            if let Err(err) = incumbent.start_kill() {
                debug!(module_id, error = %err, "swapped-out incumbent kill failed; it may already have exited");
            }
            incumbent.wait().await
        }
    };
    let exit_report = match status {
        Ok(status) => classify_exit(&status),
        Err(err) => {
            warn!(module_id, error = %err, "could not reap the swapped-out incumbent");
            wait_error_exit_report()
        }
    };
    incumbent.drain_stderr(module_id).await;
    runtime.spawn_events.emit_superseded_exited(
        module_id,
        incumbent_generation,
        incumbent.pid,
        exit_report.code,
        exit_report.signal,
    );
    {
        let record = TerminalRecord {
            exit_code: exit_report.code,
            exit_signal: exit_report.signal,
            at_ms: exit_report.at_ms,
            disposition: TerminalDisposition::Restarting,
            exit_kind: exit_report.kind.into(),
            disposition_detail: Some("replaced by a blue/green swap".to_string()),
        };
        // Recorded as `daemon_shutdown` instead if the daemon has begun
        // shutting down (see `TerminalRing::record_exit`).
        runtime
            .terminal_ring
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record_exit(module_id, record);
    }
    let _ = update_snapshot(snapshot, Some(module_id), |state| {
        state.last_exit = Some(exit_report.clone());
    });
    if let Err(err) = wait_for_slot_registration_release(
        registry,
        RegistrationSlot::Connection(incumbent_connection),
        REGISTRY_RELEASE_TIMEOUT,
    )
    .await
    {
        warn!(module_id, error = %err, "swapped-out incumbent's registration outlived its process");
    }
    info!(
        module_id,
        exit_code = ?exit_report.code,
        exit_signal = ?exit_report.signal,
        "swapped-out incumbent drained and exited; swap complete"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// During a swap two processes of the module are alive, and consumers
    /// started by either one attest with that process's launch nonce. Both are
    /// accepted while the swap is open, including after cutover has made the
    /// candidate's nonce the recorded one; once the swap closes, only the
    /// recorded nonce is.
    #[test]
    fn consumer_attestation_accepts_both_nonces_only_while_the_swap_is_open() {
        let handle = SupervisorHandle::new();
        handle.set_spawn_nonce("aft", "incumbent".to_string());
        assert!(!handle.spawned_consumer_authorized("aft", "candidate"));

        handle.open_swap("aft", "candidate".to_string());
        assert!(handle.spawned_consumer_authorized("aft", "incumbent"));
        assert!(handle.spawned_consumer_authorized("aft", "candidate"));
        assert!(!handle.spawned_consumer_authorized("aft", "forged"));

        handle.promote_swap_nonce("aft", false);
        assert!(
            handle.spawned_consumer_authorized("aft", "incumbent"),
            "the draining incumbent's consumers must keep attesting after cutover"
        );
        assert!(handle.spawned_consumer_authorized("aft", "candidate"));

        handle.close_swap("aft");
        assert!(handle.spawned_consumer_authorized("aft", "candidate"));
        assert!(!handle.spawned_consumer_authorized("aft", "incumbent"));
    }

    /// A module has two cgroup names, and no module id, however it is
    /// spelled, can produce another module's name of either kind. The ids
    /// here are the ones the cgroup library's own encoding merges: an `@`, its
    /// escape spelled out literally, and an id ending in the swap suffix.
    #[test]
    fn cgroup_names_are_injective_across_ids_and_slots() {
        assert_eq!(cgroup_name("aft", false), "aft");
        assert_eq!(cgroup_name("aft", true), "aft_swap");
        let ids = [
            "aft",
            "aft@swap",
            "aft_40swap",
            "aft_swap",
            "aft_",
            "mcp:x",
            "mcp_3ax",
        ];
        let mut names = std::collections::HashSet::new();
        for id in ids {
            for alternate in [false, true] {
                assert!(
                    names.insert(cgroup_name(id, alternate)),
                    "{id:?} (alternate: {alternate}) names a directory another id or slot already has: {}",
                    cgroup_name(id, alternate)
                );
            }
        }
    }
}
