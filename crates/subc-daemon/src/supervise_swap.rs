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

#[path = "supervise/swap_terminal.rs"]
mod swap_terminal;

// Gate publication can spend several seconds in disk-bound work under fsync load.
// Allow that work the same 30 s as a live config ACK, but do not retain a warmed
// candidate indefinitely if cutover still cannot acquire the gate.
const SWAP_CUTOVER_DEADLINE: Duration = Duration::from_secs(30);

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
/// not a hex digit) can only be the suffix. Placement then escapes `_` again:
/// this label is not yet the filesystem component.
#[cfg(any(target_os = "linux", test))]
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
    /// A newer approved configuration requires a launch the candidate missed.
    Superseded {
        generation: u64,
    },
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
    /// Commands for the loop to run next. An interrupting stop, disable or hold
    /// takes priority over retained configuration commands so a queued restart
    /// cannot run before the stop is applied. Otherwise arrival order is kept.
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
    fn into_error(self, module_id: &str, exit: ExitReport) -> SuperviseError {
        SuperviseError::SwapFailed {
            module_id: module_id.to_string(),
            arm: self.arm,
            detail: self.detail,
            candidate_exit: Some(exit),
        }
    }
}

/// Run one swap through cutover or failure. Successful cutover starts
/// incumbent retirement in the background and replies on `reply` immediately.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_swap(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Arc<Registry>,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<SupervisedChild>,
    commands: &mut mpsc::Receiver<SupervisorCommand>,
    ready_timeout: Duration,
    reply: oneshot::Sender<Result<SwapOutcome, SuperviseError>>,
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
    registry: &Arc<Registry>,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<SupervisedChild>,
    commands: &mut mpsc::Receiver<SupervisorCommand>,
    ready_timeout: Duration,
    reply: oneshot::Sender<Result<SwapOutcome, SuperviseError>>,
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
    let mut candidate = loop {
        match spawn_child_in_slot_async(
            spec,
            runtime,
            SpawnRole::SwapCandidate,
            candidate_alternate,
        )
        .await
        {
            Ok(candidate) => break candidate,
            Err(SuperviseError::ConfigDeliveryDeferred { .. }) => {
                if wait_for_config_gate(runtime, module_id, commands, end, None).await {
                    continue;
                }
                handle.close_swap(module_id);
                let _ = reply.send(Err(SuperviseError::SwapFailed {
                    module_id: module_id.to_owned(),
                    arm: SwapFailureArm::Interrupted,
                    detail: "swap interrupted while configuration publication was pending".into(),
                    candidate_exit: None,
                }));
                return;
            }
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
        }
    };
    #[cfg(target_os = "macos")]
    candidate.confirm_privacy_exec().await;
    #[cfg(target_os = "linux")]
    let candidate_cgroup = candidate
        .cgroup_placement
        .as_ref()
        .map(|_| candidate.module_id.as_str());
    #[cfg(not(target_os = "linux"))]
    let candidate_cgroup: Option<&str> = None;
    info!(
        module_id,
        candidate_pid = candidate.pid,
        cgroup = ?candidate_cgroup,
        incumbent_connection_id = incumbent_connection.get(),
        ready_timeout_ms = ready_timeout.as_millis() as u64,
        "swap candidate spawned; routing stays on the incumbent until it is ready"
    );

    let config_deadline = config_candidate_deadline(runtime, &candidate);
    let warm = warm_candidate(
        module_id,
        registry,
        &mut candidate,
        incumbent_connection,
        ready_timeout,
        config_deadline,
        runtime.config_delivery.as_ref(),
        commands,
        end,
    )
    .await;
    let candidate_connection = match warm {
        Warm::Superseded { generation } => {
            abandon_candidate(module_id, registry, &forwarding, runtime, candidate, None).await;
            update_snapshot(snapshot, Some(module_id), |state| {
                state.swap_outcome = Some(SwapOutcome::SupersededByConfigRestart)
            })
            .ok();
            end.requeue.push(SupervisorCommand::ConfigApplyRestart {
                generation,
                supersedes_swap: true,
            });
            let _ = reply.send(Ok(SwapOutcome::SupersededByConfigRestart));
            return;
        }
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
            let exit = abandon_candidate(
                module_id,
                registry,
                &forwarding,
                runtime,
                candidate,
                Some(&failure),
            )
            .await;
            let _ = reply.send(Err(failure.into_error(module_id, exit)));
            // Stop/disable/hold must take effect before retained restart
            // commands; the actor then discards restarts for the stopped child.
            if let Some(command) = command {
                end.requeue.insert(0, command);
            }
            return;
        }
        Warm::Failed(failure) => {
            let exit = abandon_candidate(
                module_id,
                registry,
                &forwarding,
                runtime,
                candidate,
                Some(&failure),
            )
            .await;
            let _ = reply.send(Err(failure.into_error(module_id, exit)));
            return;
        }
    };

    if let Err(failure) = probe_candidate(
        module_id,
        runtime,
        registry,
        &forwarding,
        config_deadline,
        candidate_connection,
    )
    .await
    {
        let exit = abandon_candidate(
            module_id,
            registry,
            &forwarding,
            runtime,
            candidate,
            Some(&failure),
        )
        .await;
        let _ = reply.send(Err(failure.into_error(module_id, exit)));
        return;
    }

    let config_owner = runtime
        .config_delivery
        .as_ref()
        .map(|delivery| delivery.0.clone());
    let config_gate = match &config_owner {
        Some(owner) => match owner.module_gate(module_id) {
            Ok(gate) => Some(gate),
            Err(error) => {
                let failure = CandidateFailure {
                    arm: SwapFailureArm::CandidateUnhealthy,
                    detail: format!("config_gated: {error}"),
                    exit: None,
                    connection: Some(candidate_connection),
                };
                let exit = abandon_candidate(
                    module_id,
                    registry,
                    &forwarding,
                    runtime,
                    candidate,
                    Some(&failure),
                )
                .await;
                let _ = reply.send(Err(failure.into_error(module_id, exit)));
                return;
            }
        },
        None => None,
    };
    let config_opted_in = registry
        .registration(RegistrationSlot::Connection(candidate_connection))
        .ok()
        .flatten()
        .is_some_and(|registration| {
            registration
                .control_ops
                .iter()
                .any(|op| op == "config.review")
        });
    let mut cutover_deadline = None;
    let mut candidate_exit = None;
    let (forwarding_cutover, promoted) = loop {
        let attempt = if cutover_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            Err(CutoverDeferred::DeadlineExceeded)
        } else {
            try_config_cutover(
                module_id,
                config_owner.as_ref(),
                config_gate.as_ref(),
                config_opted_in,
                candidate.config_delivery.as_ref(),
                candidate_connection,
                registry,
                &forwarding,
            )
        };
        match attempt {
            Ok(cutover) => break cutover,
            Err(CutoverDeferred::Busy) => {
                let deadline =
                    *cutover_deadline.get_or_insert_with(|| Instant::now() + SWAP_CUTOVER_DEADLINE);
                let retry = tokio::select! {
                    ready=wait_for_config_gate(runtime,module_id,commands,end,Some(deadline))=>ready,
                    status=candidate.wait()=>{
                        candidate_exit = Some(swap_terminal::observed_exit(module_id, status));
                        false
                    },
                    _=async {match child.as_mut() {Some(incumbent)=>{let _=incumbent.wait().await;},None=>std::future::pending::<()>().await}}=>false,
                };
                if retry || Instant::now() >= deadline {
                    continue;
                }
                let failure = CandidateFailure {
                    arm: SwapFailureArm::Interrupted,
                    detail: "swap interrupted before configuration cutover".into(),
                    exit: candidate_exit.take(),
                    connection: Some(candidate_connection),
                };
                let exit = abandon_candidate(
                    module_id,
                    registry,
                    &forwarding,
                    runtime,
                    candidate,
                    Some(&failure),
                )
                .await;
                let _ = reply.send(Err(failure.into_error(module_id, exit)));
                return;
            }
            Err(CutoverDeferred::NewGeneration(generation)) => {
                abandon_candidate(module_id, registry, &forwarding, runtime, candidate, None).await;
                update_snapshot(snapshot, Some(module_id), |state| {
                    state.swap_outcome = Some(SwapOutcome::SupersededByConfigRestart)
                })
                .ok();
                end.requeue.push(SupervisorCommand::ConfigApplyRestart {
                    generation,
                    supersedes_swap: true,
                });
                let _ = reply.send(Ok(SwapOutcome::SupersededByConfigRestart));
                return;
            }
            Err(failed) => {
                let restart = matches!(failed, CutoverDeferred::RegistryLost);
                let failure = CandidateFailure {
                    arm: if matches!(failed, CutoverDeferred::Refused(_)) {
                        SwapFailureArm::CandidateUnhealthy
                    } else {
                        SwapFailureArm::CutoverLost
                    },
                    detail: match failed {
                        CutoverDeferred::DeadlineExceeded => {
                            "configuration cutover deadline expired while the gate was busy".into()
                        }
                        CutoverDeferred::Refused(error) => format!("config_gated: {error}"),
                        CutoverDeferred::RegistryLost => {
                            "the candidate registration closed during cutover; restarting plainly"
                                .into()
                        }
                        _ => "the candidate connection closed just before cutover".into(),
                    },
                    exit: candidate_exit.take(),
                    connection: Some(candidate_connection),
                };
                let exit = abandon_candidate(
                    module_id,
                    registry,
                    &forwarding,
                    runtime,
                    candidate,
                    Some(&failure),
                )
                .await;
                let _ = reply.send(Err(failure.into_error(module_id, exit)));
                if restart {
                    if let Err(error) = restart_child(
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
                        warn!(module_id,%error,"plain restart after a lost swap cutover failed");
                        fail_snapshot(snapshot, Some(module_id), None, &FailureContext::operator());
                    }
                }
                return;
            }
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
    let activity_spawn = runtime.begin_activity_spawn(module_id);
    if let Err(err) = set_running(snapshot, &candidate, module_id, &runtime.spawn_events) {
        error!(module_id, error = %err, "failed to record the promoted swap candidate");
    }
    let _ = update_snapshot(snapshot, Some(module_id), |state| {
        state.in_alternate_slot = candidate_alternate;
    });
    if let Some(spawn) = activity_spawn {
        spawn.complete(snapshot);
    }
    handle.record_activity_registration(module_id);
    process_liveness.track(module_id.to_string(), Arc::clone(snapshot));
    runtime.record_activity_transition(
        "subc.module_swapped",
        module_id,
        runtime.start_requester.clone(),
    );
    let incumbent = child.replace(candidate);
    info!(
        module_id,
        promoted_connection_id = candidate_connection.get(),
        superseded_connection_id = incumbent_connection.get(),
        "swap cut over; new routes land on the promoted candidate, draining the incumbent"
    );
    update_snapshot(snapshot, Some(module_id), |state| {
        state.swap_outcome = Some(SwapOutcome::Cutover)
    })
    .ok();
    let _ = reply.send(Ok(SwapOutcome::Cutover));

    dispatch_retirement(
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
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_retirement(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Arc<Registry>,
    forwarding: &Arc<ForwardingTable>,
    snapshot: &SharedSnapshot,
    incumbent: Option<SupervisedChild>,
    incumbent_endpoint: Option<crate::ModuleEndpointId>,
    incumbent_connection: ConnectionId,
    incumbent_generation: u64,
) {
    let spec = spec.clone();
    let runtime = runtime.clone();
    let registry = Arc::clone(registry);
    let forwarding = Arc::clone(forwarding);
    let snapshot = Arc::clone(snapshot);
    // The promoted process must return to its supervision loop at cutover.
    // Keep the swap open until the old process is reaped so another swap
    // cannot reuse its slot or discard its consumer attestation nonce.
    tokio::spawn(async move {
        retire_incumbent(
            &spec,
            &runtime,
            &registry,
            &forwarding,
            &snapshot,
            incumbent,
            incumbent_endpoint,
            incumbent_connection,
            incumbent_generation,
        )
        .await;
        if let Some(handle) = &runtime.supervisor_handle {
            handle.close_swap(&spec.module_id);
        }
    });
}

enum CutoverDeferred {
    Busy,
    DeadlineExceeded,
    NewGeneration(u64),
    Refused(crate::config_gate::GateError),
    ForwardingLost,
    RegistryLost,
}

/// Publish forwarding first, registry second and the config ACK target last.
/// Both configuration guards are acquired without waiting, and only verified
/// in-memory bytes are read until both guards have been released.
#[allow(clippy::too_many_arguments)]
fn try_config_cutover(
    module: &str,
    owner: Option<&Arc<crate::config_gate::runtime::RuntimeOwner>>,
    gate: Option<&Arc<tokio::sync::Mutex<()>>>,
    opted_in: bool,
    spawn: Option<&crate::config_gate::runtime::DeliverySpawn>,
    connection: ConnectionId,
    registry: &Registry,
    forwarding: &ForwardingTable,
) -> Result<
    (
        crate::forwarding::ForwardingCutover,
        crate::registry::ModuleRegistration,
    ),
    CutoverDeferred,
> {
    let _gate = gate
        .map(|gate| {
            gate.clone()
                .try_lock_owned()
                .map_err(|_| CutoverDeferred::Busy)
        })
        .transpose()?;
    if let Some(owner) = owner {
        owner.check_module_hold(module).map_err(|error| {
            if error.code == "config_delivery_deferred" {
                CutoverDeferred::Busy
            } else {
                CutoverDeferred::Refused(error)
            }
        })?;
    }
    let mut configuration = if let Some(owner) = owner.filter(|_| opted_in) {
        #[cfg(test)]
        owner
            .swap_preflight_attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Some(
            owner
                .try_swap_configuration()
                .map_err(|_| CutoverDeferred::Busy)?,
        )
    } else {
        None
    };
    if let Some(configuration) = &mut configuration {
        if let Some(generation) = configuration
            .before_cutover(module, connection, spawn)
            .map_err(CutoverDeferred::Refused)?
        {
            return Err(CutoverDeferred::NewGeneration(generation));
        }
    }
    let forwarded = forwarding
        .cutover_candidate(module)
        .ok()
        .flatten()
        .ok_or(CutoverDeferred::ForwardingLost)?;
    #[cfg(feature = "test-support")]
    if let Some(owner) = owner {
        if let Err(error) = owner.pause_swap_before_promotion() {
            warn!(module,%error,"swap promotion test pause failed");
        }
    }
    let promoted = registry
        .promote_candidate(module)
        .ok()
        .flatten()
        .ok_or(CutoverDeferred::RegistryLost)?
        .promoted;
    if let Some(configuration) = &mut configuration {
        if let Err(error) = configuration.promoted(module, connection, Instant::now()) {
            warn!(module,%error,"promoted configuration target is gated");
        }
    }
    drop(configuration);
    if let Some(owner) = owner {
        owner.notify_delivery_changed();
    }
    Ok((forwarded, promoted))
}

async fn wait_for_config_gate(
    runtime: &SupervisorRuntimeConfig,
    module: &str,
    commands: &mut mpsc::Receiver<SupervisorCommand>,
    end: &mut SwapEnd,
    deadline: Option<Instant>,
) -> bool {
    let expired = async {
        match deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(expired);
    let backoff = tokio::time::sleep(CONFIG_ADMISSION_RETRY_DELAY);
    tokio::pin!(backoff);
    loop {
        tokio::select! {
            _=&mut expired=>return false,
            _=&mut backoff=>break,
            command=commands.recv()=>{
                let Some(command)=command else {return false;};
                if let Some(interrupt)=serve_command_while_warming(module,command,end,runtime.config_delivery.as_ref().is_some_and(|delivery|delivery.0.module_is_gated(module))) {end.requeue.push(interrupt);return false;}
            }
        }
    }
    let Some(delivery) = &runtime.config_delivery else {
        return true;
    };
    let Ok(gate) = delivery.0.module_gate(module) else {
        return false;
    };
    loop {
        tokio::select! {
            _=&mut expired=>return false,
            guard = gate.lock() => { drop(guard); return true; }
            command = commands.recv() => {
                let Some(command) = command else { return false; };
                if let Some(interrupt) = serve_command_while_warming(module, command, end,runtime.config_delivery.as_ref().is_some_and(|delivery|delivery.0.module_is_gated(module))) {
                    end.requeue.push(interrupt);
                    return false;
                }
            }
        }
    }
}

/// Answer non-interrupting commands while a swap waits, or return a stop,
/// disable, retire, configuration restart or configuration hold command for
/// the module loop to apply.
/// Live file updates cannot replace the configuration saved by operator approval.
fn serve_command_while_warming(
    module_id: &str,
    command: SupervisorCommand,
    end: &mut SwapEnd,
    gated: bool,
) -> Option<SupervisorCommand> {
    let in_progress = || SuperviseError::SwapInProgress {
        module_id: module_id.to_string(),
    };
    match command {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        SupervisorCommand::Upgrade { request, .. } => {
            request.refuse_running_swap(module_id);
            None
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        SupervisorCommand::HeldAdoptionResolved { .. } => {
            end.requeue.push(command);
            None
        }
        SupervisorCommand::ConfigPolicyProbe { .. } => {
            end.requeue.push(command);
            None
        }
        SupervisorCommand::ConfigApplyRestart { .. } => Some(command),
        SupervisorCommand::Drain { .. }
        | SupervisorCommand::ApprovedConfiguration { .. }
        | SupervisorCommand::ConfigExitHold { .. }
        | SupervisorCommand::ConfigDeliveryFailed { .. }
        | SupervisorCommand::Retire { .. }
        | SupervisorCommand::SetEnabled { enabled: false, .. } => Some(command),
        SupervisorCommand::SetEnabled {
            enabled: true,
            reply,
            ..
        } => {
            let _ = reply.send(Ok(false));
            None
        }
        SupervisorCommand::Restart { reply, .. } | SupervisorCommand::Reload { reply, .. } => {
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
        SupervisorCommand::UpdateConfiguration { reply, .. } if gated => {
            let _ = reply.send(Err(SuperviseError::ConfigHeld {
                module_id: module_id.into(),
                code: "config_gated".into(),
                detail: "live file reconfiguration cannot replace stamped authority".into(),
            }));
            None
        }
        SupervisorCommand::UpdateConfiguration {
            spec,
            health,
            drain_timeout_ms,
            reply,
        } => {
            let _ = reply.send(Ok(()));
            // A swap keeps the drain budget it started with, even if it hasn't
            // begun draining the incumbent yet. Replay the update after the swap
            // so subsequent drains use it. The caller already has its answer.
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
    config_deadline: Option<Instant>,
    config_delivery: Option<&super::config_delivery::ConfigDelivery>,
    commands: &mut mpsc::Receiver<SupervisorCommand>,
    end: &mut SwapEnd,
) -> Warm {
    let requeue_start = end.requeue.len();
    let outcome = warm_candidate_until_outcome(
        module_id,
        registry,
        candidate,
        incumbent_connection,
        ready_timeout,
        config_deadline,
        config_delivery,
        commands,
        end,
    )
    .await;
    if matches!(outcome, Warm::Superseded { .. }) {
        // The superseded branch queues its freshly checked generation instead.
        // Downstream revalidation would discard a stale duplicate anyway, so
        // filtering prevents a redundant command rather than a second launch.
        let deferred = end.requeue.split_off(requeue_start);
        end.requeue.extend(
            deferred
                .into_iter()
                .filter(|command| !matches!(command, SupervisorCommand::ConfigApplyRestart { .. })),
        );
    }
    outcome
}

/// The warm-up loop proper. Every return path leaves any configuration
/// restart it consumed in `end.requeue`; [`warm_candidate`] drops those again
/// when the outcome is `Superseded`, whose caller queues a fresher generation.
#[allow(clippy::too_many_arguments)]
async fn warm_candidate_until_outcome(
    module_id: &str,
    registry: &Registry,
    candidate: &mut SupervisedChild,
    incumbent_connection: ConnectionId,
    ready_timeout: Duration,
    config_deadline: Option<Instant>,
    config_delivery: Option<&super::config_delivery::ConfigDelivery>,
    commands: &mut mpsc::Receiver<SupervisorCommand>,
    end: &mut SwapEnd,
) -> Warm {
    let mut deadline = Instant::now() + ready_timeout;
    let mut registered: Option<ConnectionId> = None;
    let mut config_restart_pending = false;
    let mut next_config_check = Instant::now();
    loop {
        if config_restart_pending && Instant::now() >= next_config_check {
            if let Some(delivery) = config_delivery {
                match delivery
                    .0
                    .try_requires_fresher_spawn(module_id, candidate.config_delivery.as_ref())
                {
                    Ok(Some(generation)) => return Warm::Superseded { generation },
                    Ok(None) => config_restart_pending = false,
                    Err(_) => {
                        #[cfg(test)]
                        if let Some(reached) = delivery.0.warm_restart_busy.lock().unwrap().take() {
                            let _ = reached.send(());
                        }
                        next_config_check = Instant::now() + CONFIG_ADMISSION_RETRY_DELAY;
                    }
                }
            } else {
                config_restart_pending = false;
            }
        }
        // Before the candidate registers, the candidate slot is the only
        // place to look for it; once it has, its connection names it exactly.
        let slot = match registered {
            Some(connection) => RegistrationSlot::Connection(connection),
            None => RegistrationSlot::Candidate(module_id),
        };
        // Only a published candidate counts as registered. A candidate's HELLO
        // reserves its registry entry before installing its forwarding
        // endpoint and publishes it after; a ready candidate seen in between
        // would be promoted while the forwarding cutover still finds no
        // candidate endpoint, and the swap would fail as if the candidate's
        // connection had closed.
        match registry.published_registration(slot) {
            Ok(Some(registration)) => {
                registered = Some(registration.connection_id);
                if registration
                    .control_ops
                    .iter()
                    .any(|op| op == "config.review")
                {
                    if let Some(cap) = config_deadline {
                        deadline = deadline.min(cap);
                        if Instant::now() >= cap {
                            return Warm::Failed(CandidateFailure {
                                arm:SwapFailureArm::NeverReady,
                                detail:"config_gated: candidate configuration warm-up deadline expired before readiness".into(),
                                exit:None, connection:registered,
                            });
                        }
                    }
                }
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
                let exit = swap_terminal::observed_exit(module_id, status);
                return Warm::Failed(CandidateFailure {
                    arm: SwapFailureArm::CandidateExited,
                    detail: candidate.spawn_failure.clone().unwrap_or_else(|| format!(
                        "the candidate exited before it was ready (code {:?}, signal {:?})",
                        exit.code, exit.signal
                    )),
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
                if matches!(command, SupervisorCommand::ConfigApplyRestart { .. }) {
                    // The coordinator sends each generation only once. Keep
                    // the command even if readiness, exit or timeout wins the
                    // race with the nonblocking freshness recheck. The actor
                    // revalidates it against the current spawn before acting.
                    end.requeue.push(command);
                    #[cfg(test)]
                    if let Some(delivery) = config_delivery {
                        let pause = delivery.0.warm_restart_pause.lock().unwrap().take();
                        if let Some(pause) = pause {
                            let _ = pause.reached.send(());
                            let _ = pause.resume.await;
                        }
                    }
                    config_restart_pending = true;
                    next_config_check = Instant::now();
                    continue;
                }
                if let Some(command) = serve_command_while_warming(module_id, command, end,config_delivery.is_some_and(|delivery|delivery.0.module_is_gated(module_id))) {
                    return Warm::Interrupted { command: Some(command), connection: registered };
                }
            }
            _ = sleep(poll) => {}
        }
    }
}

fn config_candidate_deadline(
    runtime: &SupervisorRuntimeConfig,
    candidate: &SupervisedChild,
) -> Option<Instant> {
    runtime.config_delivery.as_ref().map(|_| {
        candidate.spawned_at
            + runtime.register_timeout
            + runtime.health.deadline
            + crate::config_gate::runtime::delivery::CONFIG_WARMUP_SLACK
    })
}

/// Probe a ready candidate once, by endpoint, before promoting it.
///
/// The same `health.check` the supervisor probes every module with, sent only
/// when the candidate advertises it. A `failing` answer or no usable answer
/// fails the swap. A configuration-delivery candidate must answer healthy;
/// otherwise the existing non-config swap policy also accepts degraded service.
/// Like every other swap failure it spends no crash-restart budget: the
/// budget is the incumbent's, and the incumbent did nothing.
async fn probe_candidate(
    module_id: &str,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    forwarding: &ForwardingTable,
    config_deadline: Option<Instant>,
    candidate_connection: ConnectionId,
) -> Result<(), CandidateFailure> {
    let unhealthy = |detail: String| CandidateFailure {
        arm: SwapFailureArm::CandidateUnhealthy,
        detail,
        exit: None,
        connection: Some(candidate_connection),
    };
    let registration = registry
        .registration(RegistrationSlot::Connection(candidate_connection))
        .ok()
        .flatten();
    let config_owned = runtime.config_delivery.is_some()
        && registration.as_ref().is_some_and(|registration| {
            registration
                .control_ops
                .iter()
                .any(|op| op == "config.review")
        });
    let advertises_health = registration.as_ref().is_some_and(|registration| {
        registration
            .control_ops
            .iter()
            .any(|op| op == MODULE_CONTROL_OP_HEALTH_CHECK)
    });
    if !advertises_health {
        if config_owned {
            return Err(unhealthy(
                "config_gated: configuration readiness requires the first healthy probe".into(),
            ));
        }
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
    let deadline_cap = config_owned.then_some(config_deadline).flatten();
    match probe_endpoint_health(endpoint, runtime, deadline_cap).await {
        Ok(_) if deadline_cap.is_some_and(|cap| Instant::now() >= cap) => Err(unhealthy(
            "config_gated: candidate configuration warm-up deadline expired before its first healthy probe finished".into())),
        Ok(report) if config_owned && report.status != HealthStatus::Ok => Err(unhealthy(
            "config_gated: configuration readiness requires a healthy, not degraded or failing, probe".into())),
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
    runtime: &SupervisorRuntimeConfig,
    mut candidate: SupervisedChild,
    failure: Option<&CandidateFailure>,
) -> ExitReport {
    candidate.retire_registration();
    if let Some(failure) = failure {
        warn!(module_id, arm=failure.arm.as_str(), detail=%failure.detail, candidate_pid=candidate.pid,
            "swap failed; killing the candidate and leaving the incumbent serving");
    } else {
        info!(
            module_id,
            candidate_pid = candidate.pid,
            outcome = SwapOutcome::SupersededByConfigRestart.as_str(),
            "swap superseded by approved configuration restart; leaving the incumbent serving"
        );
    }
    let exit = swap_terminal::abandoned_exit(module_id, &mut candidate, failure).await;
    candidate.drain_stderr(module_id).await;
    swap_terminal::record_abandonment(module_id, runtime, &exit, failure);
    if let Err(error) = candidate.finish_exit(false).await {
        warn!(module_id, %error, "could not finalize the abandoned swap candidate");
    }

    let slot = match failure.and_then(|failure| failure.connection) {
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
        if let Some(connection) = failure.and_then(|failure| failure.connection).or_else(|| {
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
                    format!("swap of module '{module_id}' ended; closing its candidate"),
                ),
            );
        }
    }
    if let Some(handle) = &runtime.supervisor_handle {
        handle.close_swap(module_id);
    }
    exit
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
pub(super) async fn retire_incumbent(
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
    let mut incumbent = incumbent;
    let mut observed_exit = None;
    let stop_notice = if let Some(endpoint) = incumbent_endpoint {
        let drain = begin_forwarding_drain_with(
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
        );
        let result = if let Some(child) = incumbent.as_mut() {
            tokio::select! {
                status = child.wait() => {
                    observed_exit = Some(status);
                    Ok(StopNotice::NotSent)
                }
                result = drain => result,
            }
        } else {
            drain.await
        };
        result.unwrap_or_else(|err| {
            warn!(module_id, error = %err, "draining the swapped-out incumbent failed; stopping it anyway");
            StopNotice::NotSent
        })
    } else {
        StopNotice::NoConnection
    };

    let Some(mut incumbent) = incumbent else {
        return;
    };
    // Cutover already revoked authority. Keep the transport for in-flight
    // replies and drain notices until its bounded GOODBYE flush completes.
    incumbent.retire_registration();
    if observed_exit.is_none() && stop_notice != StopNotice::SentOverConnection {
        request_graceful_stop(module_id, &incumbent);
    }
    // The module GOODBYE above asks the incumbent to exit; give it the same
    // budget a plain restart gives the child it drains, then kill it.
    let status = match observed_exit {
        Some(status) => status,
        None => match timeout(
            incumbent.group_drain_budget(runtime.drain_timeout, stop_notice),
            incumbent.wait(),
        )
        .await
        {
            Ok(status) => status,
            Err(_) => {
                warn!(
                    module_id,
                    pid = incumbent.pid,
                    budget_ms =
                        u64::try_from(runtime.drain_timeout.as_millis()).unwrap_or(u64::MAX),
                    reason = "swapped-out incumbent",
                    "drain budget expired before the module exited; killing it"
                );
                if let Err(err) = incumbent.start_kill() {
                    debug!(module_id, error = %err, "swapped-out incumbent kill failed; it may already have exited");
                }
                incumbent.wait().await
            }
        },
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
            disposition_detail: incumbent
                .exit_cleanup_detail(Some("replaced by a blue/green swap".into())),
        };
        // Recorded as `daemon_shutdown` instead if the daemon has begun
        // shutting down (see `TerminalRing::record_exit`).
        runtime
            .terminal_ring
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record_exit(module_id, record);
    }
    // Shutdown waits for the roster to empty before flushing the journal.
    // Enqueue the record before releasing this roster entry.
    if let Err(error) = incumbent.finish_exit(false).await {
        warn!(module_id, %error, "could not finalize the swapped-out incumbent");
    }
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn running_swap_refuses_upgrade_without_interrupting_candidate() {
        let (reply, response) = oneshot::channel();
        let mut end = SwapEnd::default();
        let request = SupervisorCommand::Upgrade {
            key: Arc::new(()),
            request: crate::supervise::live_upgrade::ActorRequest::Select(reply),
        };
        assert!(serve_command_while_warming("candidate", request, &mut end, false).is_none());
        let refusal = response.await.unwrap().unwrap_err();
        assert_eq!(refusal.kind(), std::io::ErrorKind::WouldBlock);
        assert!(refusal
            .to_string()
            .contains("module 'candidate' is swapping"));
        assert!(end.requeue.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn config_admission_swap_cannot_ack_or_queue_an_unapproved_configuration_update() {
        let (_rig, _owner, _supervisor, _handler, spec) =
            crate::config_gate::runtime::RuntimeOwner::unactivated_supervisor_fixture_for_test();
        let (reply, answer) = oneshot::channel();
        let mut end = SwapEnd::default();
        assert!(serve_command_while_warming(
            &spec.module_id.clone(),
            SupervisorCommand::UpdateConfiguration {
                spec,
                health: HealthConfig::default(),
                drain_timeout_ms: None,
                reply
            },
            &mut end,
            true
        )
        .is_none());
        assert!(matches!(
            answer.await.unwrap(),
            Err(SuperviseError::ConfigHeld { .. })
        ));
        assert!(end.requeue.is_empty());
    }

    #[cfg(unix)]
    struct ConfigTimingFixture {
        dir: cortexkit_test_support::ScratchDir,
        runtime: SupervisorRuntimeConfig,
        registry: Arc<Registry>,
        forwarding: Arc<ForwardingTable>,
        child: SupervisedChild,
        receiver: mpsc::Receiver<crate::router::OutboundFrame>,
    }

    #[cfg(unix)]
    fn config_timing_fixture() -> ConfigTimingFixture {
        let dir = cortexkit_test_support::ScratchDir::new("config-swap-deadline");
        let owner = Arc::new(crate::config_gate::runtime::RuntimeOwner::new(
            "swap-deadline".into(),
            crate::config_store::ConfigStore::open_digest_only_for_tests(
                crate::config_store::StoreRoot::from_service_definition(dir.join("store")).unwrap(),
            ),
            crate::config_gate::capture::ConfigCapturer {
                daemon_path: dir.join("subc.jsonc"),
                config_home: dir.to_path_buf(),
                projects: Arc::new(crate::config_gate::capture::OfflineProjectRegistry),
            },
        ));
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
            .with_config_runtime(owner)
            .with_live_children_record(dir.join("children.json"))
            .with_forwarding(forwarding.clone());
        let mut runtime = supervisor.runtime_config();
        runtime.register_timeout = Duration::from_secs(5);
        runtime.health.deadline = Duration::from_secs(2);
        let spec = ModuleSpec {
            script: None,
            module_id: "timed".into(),
            program: PathBuf::from("/bin/sleep"),
            args: vec!["600".into()],
            env: vec![],
            reserved: false,
            reserved_prefixes: vec![],
            protocol: ModuleProtocol::None,
            overlap: ModuleOverlap::Safe,
            carry_containment: false,
        };
        let candidate = spawn_child(
            &spec,
            None,
            runtime.supervisor_handle.as_ref(),
            &runtime.stderr_ring,
            None,
            &runtime.child_roster,
            runtime.spawn_delivery(),
            #[cfg(target_os = "linux")]
            None,
            runtime.register_timeout,
        )
        .unwrap();
        let mut manifest =
            subc_protocol::manifest::ModuleManifest::builder("timed", "1.0.0").build();
        manifest.ready = Some(true);
        registry
            .register_candidate_with_control_ops(
                manifest,
                subc_protocol::PROTOCOL_VERSION,
                ConnectionId::new(2),
                vec![
                    "config.review".into(),
                    MODULE_CONTROL_OP_HEALTH_CHECK.into(),
                ],
            )
            .unwrap();
        let (sender, receiver) = mpsc::channel(8);
        forwarding
            .register_candidate_module_connection(
                ConnectionId::new(2),
                "timed".into(),
                subc_protocol::PROTOCOL_VERSION,
                subc_protocol::manifest::Concurrency::ModuleManaged,
                FrameSink::new(sender),
            )
            .unwrap();
        ConfigTimingFixture {
            dir,
            runtime,
            registry,
            forwarding,
            child: candidate,
            receiver,
        }
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn config_admission_swap_retry_has_a_bounded_backoff_even_when_gate_is_free() {
        let fixture = config_timing_fixture();
        let (_tx, mut commands) = mpsc::channel(8);
        let mut end = SwapEnd::default();
        let mut wait = Box::pin(wait_for_config_gate(
            &fixture.runtime,
            "timed",
            &mut commands,
            &mut end,
            None,
        ));
        for advance in [Duration::ZERO, Duration::from_millis(99)] {
            tokio::time::advance(advance).await;
            std::future::poll_fn(|cx| {
                assert!(
                    std::future::Future::poll(wait.as_mut(), cx).is_pending(),
                    "a free gate must not spin configuration-deferred candidate launches"
                );
                std::task::Poll::Ready(())
            })
            .await;
        }
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(wait.await);
        finish_timing_child(fixture.child).await;
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn config_admission_swap_busy_gate_wait_uses_absolute_deadline() {
        let fixture = config_timing_fixture();
        let gate = fixture
            .runtime
            .config_delivery
            .as_ref()
            .unwrap()
            .0
            .module_gate("timed")
            .unwrap();
        let _guard = gate.lock().await;
        let (_sender, mut commands) = mpsc::channel(8);
        let mut end = SwapEnd::default();
        let deadline = Instant::now() + SWAP_CUTOVER_DEADLINE;
        let mut wait = Box::pin(wait_for_config_gate(
            &fixture.runtime,
            "timed",
            &mut commands,
            &mut end,
            Some(deadline),
        ));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(wait.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        tokio::time::advance(SWAP_CUTOVER_DEADLINE - Duration::from_nanos(1)).await;
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(wait.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        tokio::time::advance(Duration::from_nanos(1)).await;
        let expired = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(matches!(
                std::future::Future::poll(wait.as_mut(), cx),
                std::task::Poll::Ready(false)
            ))
        })
        .await;
        drop(wait);
        finish_timing_child(fixture.child).await;
        assert!(expired, "the config gate lock await must end at the cutover deadline, even while the mutex stays held");
    }

    #[cfg(unix)]
    async fn busy_cutover_failure(
        hold_module_gate: bool,
    ) -> (
        Vec<subc_config::Generation>,
        SupervisorSnapshot,
        SupervisorSnapshot,
    ) {
        let mut fixture = config_timing_fixture();
        approve_timing_restart(&fixture);
        let owner = fixture.runtime.config_delivery.as_ref().unwrap().0.clone();
        fixture.runtime.supervisor_handle = Some(SupervisorHandle::new());
        let mut manifest = fixture
            .registry
            .get_candidate("timed")
            .unwrap()
            .unwrap()
            .manifest;
        fixture
            .registry
            .deregister_connection(ConnectionId::new(2))
            .unwrap();
        fixture
            .registry
            .register_with_control_ops(
                manifest.clone(),
                subc_protocol::PROTOCOL_VERSION,
                ConnectionId::new(1),
                vec![],
            )
            .unwrap();
        manifest.ready = Some(true);
        fixture
            .registry
            .register_candidate_with_control_ops(
                manifest,
                subc_protocol::PROTOCOL_VERSION,
                ConnectionId::new(2),
                vec![
                    "config.review".into(),
                    MODULE_CONTROL_OP_HEALTH_CHECK.into(),
                ],
            )
            .unwrap();
        let (incumbent_sender, _incumbent_receiver) = mpsc::channel(8);
        fixture
            .forwarding
            .register_module_connection(
                ConnectionId::new(1),
                "timed".into(),
                subc_protocol::PROTOCOL_VERSION,
                subc_protocol::manifest::Concurrency::ModuleManaged,
                FrameSink::new(incumbent_sender),
            )
            .unwrap();
        let spec = ModuleSpec {
            script: None,
            module_id: "timed".into(),
            program: PathBuf::from("/bin/sleep"),
            args: vec!["600".into()],
            env: vec![],
            reserved: false,
            reserved_prefixes: vec![],
            protocol: ModuleProtocol::Subc,
            overlap: ModuleOverlap::Safe,
            carry_containment: false,
        };
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        set_running(
            &snapshot,
            &fixture.child,
            "timed",
            &fixture.runtime.spawn_events,
        )
        .unwrap();
        snapshot
            .lock()
            .unwrap()
            .record_crash_restart(&RestartPolicy::default(), Instant::now());
        let before = snapshot.lock().unwrap().clone();
        let mut child = Some(fixture.child);
        let incumbent_pid = child.as_ref().unwrap().pid;
        let (_sender, mut commands) = mpsc::channel(8);
        let (reply, mut answer) = oneshot::channel();
        let liveness = SupervisorProcessLiveness::default();
        let mut swap = Box::pin(run_swap(
            &spec,
            &fixture.runtime,
            &fixture.registry,
            &liveness,
            &snapshot,
            &mut child,
            &mut commands,
            Duration::from_secs(5),
            reply,
        ));
        let probe = tokio::select! {
            frame = fixture.receiver.recv() => frame.unwrap(),
            _ = &mut swap => panic!("swap ended before probing the candidate"),
        };
        assert_eq!(fixture.runtime.child_roster.live_count_for_test(), 2);
        let candidate_pid = crate::live_children::read_record(&fixture.dir.join("children.json"))
            .unwrap()
            .into_iter()
            .find(|entry| entry.pid != incumbent_pid)
            .unwrap()
            .pid;
        let gate = owner.module_gate("timed").unwrap();
        let _gate_guard = if hold_module_gate {
            Some(gate.lock().await)
        } else {
            None
        };
        let owner_guard = if hold_module_gate {
            None
        } else {
            Some(owner.try_swap_configuration().unwrap())
        };
        fixture
            .forwarding
            .complete_module_control_rpc(
                ConnectionId::new(2),
                probe.header.corr,
                Some(MODULE_CONTROL_OP_HEALTH_CHECK),
                ModuleControlRpcOutcome::Response(
                    subc_protocol::session::ModuleControlResponse::HealthCheck {
                        status: HealthStatus::Ok,
                        detail: None,
                        metrics: None,
                    },
                ),
            )
            .unwrap();
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(swap.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        // Exercise multiple retries when only the owner's state is busy, as well
        // as the single parked lock await when the module gate is held.
        for advance in [
            SWAP_CUTOVER_DEADLINE / 2,
            SWAP_CUTOVER_DEADLINE / 2 - Duration::from_nanos(1),
        ] {
            tokio::time::advance(advance).await;
            std::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(swap.as_mut(), cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            assert!(
                matches!(answer.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
                "a busy cutover must retain its candidate until the deadline"
            );
        }
        tokio::time::advance(Duration::from_nanos(1)).await;
        // The fake registration has no socket to observe EOF and deregister it.
        fixture
            .registry
            .deregister_connection(ConnectionId::new(2))
            .unwrap();
        tokio::time::resume();
        let end = timeout(Duration::from_secs(2), &mut swap)
            .await
            .expect("cutover expiry must clean up while the config lock remains held");
        drop(swap);
        assert!(matches!(
            answer.try_recv().unwrap(),
            Err(SuperviseError::SwapFailed {
                arm: SwapFailureArm::CutoverLost,
                ..
            })
        ));
        assert!(end.requeue.is_empty());
        assert_eq!(
            fixture.runtime.child_roster.live_count_for_test(),
            1,
            "candidate must be reaped and released from the roster"
        );
        assert_eq!(
            subc_os::child_exit::observe(candidate_pid)
                .unwrap_err()
                .raw_os_error(),
            Some(rustix::io::Errno::CHILD.raw_os_error()),
            "candidate must be killed and reaped, not just removed from the roster"
        );
        assert_eq!(child.as_ref().unwrap().pid, incumbent_pid);
        assert!(
            child.as_mut().unwrap().child.try_wait().unwrap().is_none(),
            "incumbent process must keep serving"
        );
        assert_eq!(
            fixture
                .registry
                .get_module("timed")
                .unwrap()
                .unwrap()
                .connection_id,
            ConnectionId::new(1)
        );
        let (client_sender, _client_receiver) = mpsc::channel(8);
        let route = fixture
            .forwarding
            .begin_route_bind_relay_for_test(
                ConnectionId::new(3),
                FrameSink::new(client_sender),
                1,
                "timed",
            )
            .unwrap();
        assert_eq!(
            route.endpoint.connection_id,
            ConnectionId::new(1),
            "new requests must still reach the incumbent"
        );
        assert!(!fixture
            .runtime
            .supervisor_handle
            .as_ref()
            .unwrap()
            .swap_open("timed"));
        let after = snapshot.lock().unwrap().clone();
        drop(owner_guard);
        let generations = owner.history().unwrap().0;
        finish_timing_child(child.take().unwrap()).await;
        (generations, before, after)
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn config_admission_swap_busy_cutover_keeps_generation_deliverable() {
        let (generations, _, _) = busy_cutover_failure(true).await;
        assert_eq!(generations.len(), 2);
        assert!(
            generations.iter().all(|generation| !generation
                .never_redeliver
                .iter()
                .any(|module| module == "timed")),
            "gate contention must not poison any approved generation"
        );
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn config_admission_swap_busy_cutover_preserves_restart_budget() {
        let (_, before, after) = busy_cutover_failure(true).await;
        assert_eq!(after.crash_restarts, before.crash_restarts);
        assert_eq!(after.lifetime_restarts, before.lifetime_restarts);
        assert_eq!(after.spawn_generation, before.spawn_generation);
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn config_admission_swap_busy_cutover_retries_share_one_deadline() {
        busy_cutover_failure(false).await;
    }

    #[cfg(unix)]
    async fn finish_timing_child(mut candidate: SupervisedChild) {
        candidate.start_kill().unwrap();
        candidate.wait().await.unwrap();
        candidate.finish_exit(false).await.unwrap();
    }

    #[cfg(unix)]
    fn approve_timing_restart(fixture: &ConfigTimingFixture) {
        use crate::config_gate::{ApprovalFacts, ApprovalReply, PlanKind, PresenceDecision};
        use serde_json::json;
        let owner = &fixture.runtime.config_delivery.as_ref().unwrap().0;
        std::fs::create_dir_all(fixture.dir.join("cortexkit")).unwrap();
        std::fs::write(
            fixture.dir.join("subc.jsonc"),
            serde_json::to_vec(
                &json!({"version":1,"modules":{"timed":{"program":"/bin/sleep","args":["600"],"registration_channel":true}}}),
            )
            .unwrap(),
        )
        .unwrap();
        let path = fixture.dir.join("cortexkit/timed.jsonc");
        std::fs::write(&path, br#"{"limit":1}"#).unwrap();
        owner.declare(
            ConnectionId::new(1),
            "timed",
            subc_protocol::config::ConfigDeclareRequest::new(
                json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","unevaluatedProperties":false,"x-ck-layers":["user"],"x-ck-apply":"live","properties":{"limit":{"type":"integer","description":"Limit","default":1,"x-ck-apply":"module_restart"}}}),
                path.to_string_lossy(), vec![],
            ),
            true,
        ).unwrap();
        let facts = ApprovalFacts {
            connection: 7,
            owner_uid: true,
            desktop_app: true,
            attested_operator_app: true,
            os_prompt_available: true,
            phone: None,
            caller: crate::caller_identity::CallerIdentity::default(),
        };
        for (kind, generation) in [(PlanKind::Stamp, 1), (PlanKind::Change, 2)] {
            std::fs::write(
                &path,
                serde_json::to_vec(&json!({"limit":generation})).unwrap(),
            )
            .unwrap();
            let plan = owner
                .finish_plan(
                    owner.prepare(kind).unwrap(),
                    crate::config_gate::runtime::daemon_review,
                )
                .unwrap();
            owner
                .desktop_view_delivered(ConnectionId::new(7), &plan)
                .unwrap();
            let challenge = owner.begin_approval(&plan.digest, &facts).unwrap();
            owner
                .finish_approval(
                    challenge.attempt(),
                    &facts,
                    ApprovalReply::Presence(PresenceDecision::InteractiveApproval),
                    generation,
                )
                .unwrap();
        }
        assert_eq!(
            owner
                .try_requires_fresher_spawn("timed", fixture.child.config_delivery.as_ref())
                .unwrap(),
            Some(2)
        );
    }

    #[cfg(unix)]
    async fn retained_restart_after_warm_exit(exit: &str) {
        let mut fixture = config_timing_fixture();
        fixture
            .registry
            .deregister_connection(ConnectionId::new(2))
            .unwrap();
        let mut manifest =
            subc_protocol::manifest::ModuleManifest::builder("timed", "1.0.0").build();
        fixture
            .registry
            .register_with_control_ops(
                manifest.clone(),
                subc_protocol::PROTOCOL_VERSION,
                ConnectionId::new(1),
                vec![],
            )
            .unwrap();
        manifest.ready = Some(false);
        fixture
            .registry
            .register_candidate_with_control_ops(
                manifest.clone(),
                subc_protocol::PROTOCOL_VERSION,
                ConnectionId::new(2),
                vec![],
            )
            .unwrap();
        let (sender, mut commands) = mpsc::channel(8);
        let owner = fixture.runtime.config_delivery.as_ref().unwrap().0.clone();
        let (busy_tx, mut busy_rx) = oneshot::channel();
        let owner_guard = if exit == "superseded" {
            approve_timing_restart(&fixture);
            *owner.warm_restart_busy.lock().unwrap() = Some(busy_tx);
            Some(owner.try_swap_configuration().unwrap())
        } else {
            None
        };
        sender
            .send(SupervisorCommand::ConfigApplyRestart {
                generation: 23,
                supersedes_swap: false,
            })
            .await
            .unwrap();
        let mut end = SwapEnd::default();
        let mut warm = Box::pin(warm_candidate(
            "timed",
            &fixture.registry,
            &mut fixture.child,
            ConnectionId::new(1),
            Duration::from_secs(5),
            None,
            (exit == "superseded").then_some(fixture.runtime.config_delivery.as_ref().unwrap()),
            &mut commands,
            &mut end,
        ));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(warm.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        match exit {
            "superseded" => {
                assert!(
                    busy_rx.try_recv().is_ok(),
                    "warm-up must observe the busy owner"
                );
                drop(owner_guard);
                tokio::time::advance(CONFIG_ADMISSION_RETRY_DELAY).await;
            }
            "ready" => {
                fixture
                    .registry
                    .deregister_connection(ConnectionId::new(2))
                    .unwrap();
                manifest.ready = Some(true);
                fixture
                    .registry
                    .register_candidate_with_control_ops(
                        manifest,
                        subc_protocol::PROTOCOL_VERSION,
                        ConnectionId::new(2),
                        vec![],
                    )
                    .unwrap();
            }
            "timeout" => tokio::time::advance(Duration::from_secs(5)).await,
            _ => unreachable!(),
        }
        let outcome = warm.await;
        let correct_outcome = match exit {
            "ready" => matches!(outcome, Warm::Ready(_)),
            "superseded" => matches!(outcome, Warm::Superseded { generation: 2 }),
            _ => matches!(outcome, Warm::Failed(_)),
        };
        let retained = end
            .requeue
            .iter()
            .filter(|command| {
                matches!(command, SupervisorCommand::ConfigApplyRestart {
                    generation,
                    supersedes_swap
                } if exit == "superseded" || (*generation == 23 && !*supersedes_swap))
            })
            .count();
        finish_timing_child(fixture.child).await;
        assert!(correct_outcome);
        assert_eq!(
            retained,
            usize::from(exit != "superseded"),
            "warm {exit} retained the wrong number of consumed restarts"
        );
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn config_admission_warm_ready_requeues_consumed_restart() {
        retained_restart_after_warm_exit("ready").await;
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn config_admission_warm_timeout_requeues_consumed_restart() {
        retained_restart_after_warm_exit("timeout").await;
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn config_admission_warm_superseded_drops_consumed_restart() {
        retained_restart_after_warm_exit("superseded").await;
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn config_delivery_swap_adapter_ready_uses_spawn_bound_and_rejects_the_exact_edge() {
        let ConfigTimingFixture {
            dir: _dir,
            runtime,
            registry,
            mut child,
            ..
        } = config_timing_fixture();
        let cap = config_candidate_deadline(&runtime, &child).unwrap();
        assert_eq!(
            cap.duration_since(child.spawned_at),
            Duration::from_secs(17)
        );
        tokio::time::advance(Duration::from_secs(17) - Duration::from_nanos(1)).await;
        let (_sender, mut commands) = mpsc::channel(8);
        let mut end = SwapEnd::default();
        assert!(matches!(
            warm_candidate(
                "timed",
                &registry,
                &mut child,
                ConnectionId::new(1),
                Duration::from_secs(60),
                Some(cap),
                None,
                &mut commands,
                &mut end
            )
            .await,
            Warm::Ready(_)
        ));
        tokio::time::advance(Duration::from_nanos(1)).await;
        let late = warm_candidate(
            "timed",
            &registry,
            &mut child,
            ConnectionId::new(1),
            Duration::from_secs(60),
            Some(cap),
            None,
            &mut commands,
            &mut end,
        )
        .await;
        assert!(
            matches!(late, Warm::Failed(failure) if failure.detail.contains("configuration warm-up deadline"))
        );
        finish_timing_child(child).await;
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn config_delivery_swap_adapter_healthy_probe_cannot_extend_the_spawn_budget() {
        let ConfigTimingFixture {
            dir: _dir,
            runtime,
            registry,
            forwarding,
            child,
            mut receiver,
        } = config_timing_fixture();
        let cap = config_candidate_deadline(&runtime, &child).unwrap();
        tokio::time::advance(Duration::from_secs(15)).await;
        for late in [false, true] {
            let (runtime, registry, table) =
                (runtime.clone(), registry.clone(), forwarding.clone());
            let probe = tokio::spawn(async move {
                probe_candidate(
                    "timed",
                    &runtime,
                    &registry,
                    &table,
                    Some(cap),
                    ConnectionId::new(2),
                )
                .await
            });
            let frame = receiver.recv().await.unwrap();
            tokio::time::advance(if late {
                Duration::from_nanos(1)
            } else {
                Duration::from_secs(2) - Duration::from_nanos(1)
            })
            .await;
            forwarding
                .complete_module_control_rpc(
                    ConnectionId::new(2),
                    frame.header.corr,
                    Some(MODULE_CONTROL_OP_HEALTH_CHECK),
                    ModuleControlRpcOutcome::Response(
                        subc_protocol::session::ModuleControlResponse::HealthCheck {
                            status: HealthStatus::Ok,
                            detail: None,
                            metrics: None,
                        },
                    ),
                )
                .unwrap();
            assert_eq!(
                probe.await.unwrap().is_err(),
                late,
                "only the healthy response strictly before the warm-up edge qualifies"
            );
        }
        finish_timing_child(child).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retirement_returns_promptly_and_shutdown_waits_for_its_journal() {
        let dir = cortexkit_test_support::ScratchDir::new("background-retirement");
        let journal = dir.join("terminals.jsonl");
        let supervisor =
            Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
                .with_handle(SupervisorHandle::new())
                .with_terminal_journal(journal.clone(), "retirement-test".into());
        let runtime = supervisor.runtime_config();
        let spec = ModuleSpec {
            script: None,
            carry_containment: false,
            module_id: "retiring".into(),
            program: PathBuf::from("/bin/sleep"),
            args: vec!["60".into()],
            env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
                .into_iter()
                .map(|key| (key.into(), dir.to_string_lossy().into_owned()))
                .collect(),
            reserved: false,
            reserved_prefixes: vec![],
            protocol: ModuleProtocol::None,
            overlap: ModuleOverlap::Safe,
        };
        let incumbent = spawn_child(
            &spec,
            None,
            runtime.supervisor_handle.as_ref(),
            &runtime.stderr_ring,
            None,
            &runtime.child_roster,
            runtime.spawn_delivery(),
            #[cfg(target_os = "linux")]
            None,
            runtime.register_timeout,
        )
        .unwrap();
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        let mut active_exit = wait_error_exit_report();
        active_exit.code = Some(42);
        snapshot.lock().unwrap().last_exit = Some(active_exit);
        let forwarding = Arc::new(ForwardingTable::default());
        timeout(
            Duration::from_millis(100),
            dispatch_retirement(
                &spec,
                &runtime,
                &supervisor.registry,
                &forwarding,
                &snapshot,
                Some(incumbent),
                None,
                ConnectionId::new(999),
                1,
            ),
        )
        .await
        .expect("incumbent retirement must not block the promoted process's supervision");
        supervisor.begin_daemon_shutdown();
        supervisor
            .end_children_for_daemon_shutdown(false, std::future::pending())
            .await;
        let records = std::fs::read_to_string(&journal).unwrap();
        assert!(
            records.contains("retiring"),
            "shutdown must wait for the incumbent's terminal record: {records}"
        );
        assert!(records.contains("daemon_shutdown"));
        assert_eq!(
            snapshot.lock().unwrap().last_exit.as_ref().unwrap().code,
            Some(42),
            "retirement must not replace the active lifecycle's exit"
        );
    }

    /// During a swap two processes of the module are alive, and consumers
    /// started by either one attest before cutover. Promotion ends the
    /// incumbent's authority even while its process is still draining.
    #[test]
    fn consumer_attestation_revokes_incumbent_nonce_at_promotion() {
        let handle = SupervisorHandle::new();
        handle.set_spawn_nonce("aft", "incumbent".to_string());
        assert!(!handle.spawned_consumer_authorized("aft", "candidate"));

        handle.open_swap("aft", "candidate".to_string());
        assert!(handle.spawned_consumer_authorized("aft", "incumbent"));
        assert!(handle.spawned_consumer_authorized("aft", "candidate"));
        assert!(!handle.spawned_consumer_authorized("aft", "forged"));

        handle.promote_swap_nonce("aft", false);
        assert!(
            !handle.spawned_consumer_authorized("aft", "incumbent"),
            "promotion retires the incumbent's nonce before its drain ends"
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

/// How long a swap waits for its candidate to register and declare itself
/// ready when the operator does not say. A module warming as a swap candidate
/// may take up to 90 s (aft's ceiling, the largest in the fleet), so the
/// daemon allows that plus time to start the process and send HELLO.
pub const DEFAULT_SWAP_READY_TIMEOUT: Duration = Duration::from_secs(100);

/// Told when a swap has promoted its candidate to be the module's active
/// registration.
///
/// An ordinary HELLO runs the control plane's registration side effects (the
/// capability cache, the deny census, the requirement recompute) as it
/// registers. A swap candidate's HELLO does not, because it is not routable;
/// promotion is when those must run instead, and promotion happens in the
/// supervisor, which has no other way into the control handler.
pub(crate) trait SwapPromotionObserver: Send + Sync {
    fn swap_promoted(&self, registration: &crate::registry::ModuleRegistration);
}

/// The installed [`SwapPromotionObserver`], held weakly: the observer (the
/// control handler) owns this handle, so a strong reference back would be a
/// cycle that keeps both alive.
#[derive(Clone, Default)]
pub(super) struct PromotionObserverSlot(
    pub(super) Arc<Mutex<Option<std::sync::Weak<dyn SwapPromotionObserver>>>>,
);

impl fmt::Debug for PromotionObserverSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PromotionObserverSlot")
    }
}

/// The nonces of one open swap.
#[derive(Debug, Clone)]
pub(super) struct OpenSwap {
    /// Legacy candidate token. A gated candidate has an empty lifecycle
    /// placeholder instead; only its own channel admits its HELLO.
    pub(super) candidate_nonce: String,
    /// Incumbent token accepted before cutover, then cleared by promotion even
    /// while the incumbent's process is still draining.
    pub(super) incumbent_nonce: Option<String>,
    /// Set once a HELLO has been admitted with the swap token, so the token
    /// admits one registration and cannot be replayed after cutover empties
    /// the candidate slot.
    candidate_admitted: bool,
}

/// What the swap gate says about a HELLO. See
/// [`SupervisorHandle::swap_hello_admission`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwapHelloAdmission {
    /// No swap is open for the id (or the HELLO carries the incumbent's own
    /// nonce); the ordinary gates decide.
    NotSwapping,
    /// The HELLO carries the swap token: register it into the candidate slot.
    Candidate,
    /// A swap is open and the HELLO carries a nonce the supervisor did not
    /// mint for this id, no nonce, or a token already used.
    Refused,
    /// The id belongs to a supervised `protocol: none` module. Such a module
    /// never registers, so no HELLO may claim its id, with or without a nonce.
    ProtocolNone,
    /// The id is gated. Its supervised child registers only over the
    /// registration channel it inherited at launch, never by HELLO alone.
    ChannelRequired,
}

impl SupervisorHandle {
    /// Whether `presented` is either nonce of an open swap for `module_id`.
    pub(super) fn swap_nonce_matches(&self, module_id: &str, presented: &str) -> bool {
        if self.module_is_gated(module_id) || self.protocol_none(module_id) || presented.is_empty()
        {
            return false;
        }
        let swaps = self
            .swaps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        swaps.get(module_id).is_some_and(|swap| {
            constant_time_eq(swap.candidate_nonce.as_bytes(), presented.as_bytes())
                || swap.incumbent_nonce.as_deref().is_some_and(|incumbent| {
                    constant_time_eq(incumbent.as_bytes(), presented.as_bytes())
                })
        })
    }

    /// Open a swap for `module_id` with the candidate's freshly minted nonce.
    /// Called before the candidate process exists.
    pub(crate) fn open_swap(&self, module_id: &str, candidate_nonce: String) {
        let incumbent_nonce = self
            .spawn_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(module_id)
            .cloned();
        self.swaps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                module_id.to_string(),
                OpenSwap {
                    candidate_nonce,
                    incumbent_nonce,
                    candidate_admitted: false,
                },
            );
        self.refresh_program_binding_nonces(module_id);
    }

    /// Close the swap for `module_id`, releasing whichever nonce is no longer
    /// the module's recorded one.
    pub(crate) fn close_swap(&self, module_id: &str) {
        self.swaps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(module_id);
        self.refresh_program_binding_nonces(module_id);
    }

    /// Install the observer told about swap promotions, replacing any earlier
    /// one.
    pub(crate) fn set_swap_promotion_observer(
        &self,
        observer: std::sync::Weak<dyn SwapPromotionObserver>,
    ) {
        *self
            .promotion_observer
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(observer);
    }

    /// Tell the installed observer, if it is still alive, that a swap promoted
    /// `registration`.
    fn notify_swap_promoted(&self, registration: &crate::registry::ModuleRegistration) {
        let observer = self
            .promotion_observer
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        if let Some(observer) = observer {
            observer.swap_promoted(registration);
        }
    }

    /// Whether a swap is open for `module_id`.
    pub(crate) fn swap_open(&self, module_id: &str) -> bool {
        self.swaps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(module_id)
    }

    /// Make the candidate's nonce the module's recorded spawn nonce, as a plain
    /// respawn would, once cutover has made the candidate the module's process.
    /// Promotion revokes the incumbent's tokens; the swap record stays open
    /// only for lifecycle cleanup while the old process drains.
    pub(super) fn promote_swap_nonce(&self, module_id: &str, reserved: bool) {
        let candidate_nonce = self
            .swaps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_mut(module_id)
            .map(|swap| {
                swap.incumbent_nonce = None;
                swap.candidate_nonce.clone()
            });
        self.spawn_nonces
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(module_id);
        {
            let mut nonces = self
                .reserved_nonces
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if reserved || nonces.contains_key(module_id) {
                nonces.insert(module_id.into(), None);
            }
        }
        if self.module_is_gated(module_id) {
            return;
        }
        let Some(nonce) = candidate_nonce.filter(|nonce| !nonce.is_empty()) else {
            return;
        };
        self.set_spawn_nonce(module_id, nonce.clone());
        if reserved {
            self.set_reserved_nonce(module_id, nonce);
        }
    }

    /// The swap gate for a HELLO claiming `module_id`.
    ///
    /// This runs BEFORE the reserved-module gate. A reserved module's candidate
    /// presents the candidate nonce, which the reserved gate (holding the
    /// incumbent's nonce) would refuse as `reserved_module` before swap
    /// admission was ever reached. And it applies to unreserved ids too: for an
    /// unreserved id the only thing that ever stopped a second process claiming
    /// a live id was the `duplicate_module_id` refusal, which is exactly the
    /// refusal a swap lifts for its candidate.
    ///
    /// The incumbent's own nonce falls through to the ordinary gates, which
    /// treat it as they always have (a live incumbent is refused as a
    /// duplicate). Anything else while a swap is open is refused, including an
    /// absent nonce.
    pub(crate) fn swap_hello_admission(
        &self,
        module_id: &str,
        presented: Option<&str>,
    ) -> SwapHelloAdmission {
        if self.protocol_none(module_id) {
            return SwapHelloAdmission::ProtocolNone;
        }
        if self.module_is_gated(module_id) {
            return SwapHelloAdmission::ChannelRequired;
        }
        let swaps = self
            .swaps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(swap) = swaps.get(module_id) else {
            return SwapHelloAdmission::NotSwapping;
        };
        let Some(presented) = presented.filter(|nonce| !nonce.is_empty()) else {
            return SwapHelloAdmission::Refused;
        };
        if constant_time_eq(swap.candidate_nonce.as_bytes(), presented.as_bytes()) {
            return if swap.candidate_admitted {
                SwapHelloAdmission::Refused
            } else {
                SwapHelloAdmission::Candidate
            };
        }
        if swap
            .incumbent_nonce
            .as_deref()
            .is_some_and(|incumbent| constant_time_eq(incumbent.as_bytes(), presented.as_bytes()))
        {
            return SwapHelloAdmission::NotSwapping;
        }
        SwapHelloAdmission::Refused
    }
}

impl SupervisorHandle {
    /// Exclude swap closure through the short, synchronous candidate publication.
    pub(crate) fn with_swap_candidate_admission<T>(
        &self,
        module_id: &str,
        publish: impl FnOnce() -> (T, bool),
    ) -> Option<T> {
        let mut swaps = self.swaps.lock().unwrap_or_else(|p| p.into_inner());
        let swap = swaps
            .get_mut(module_id)
            .filter(|swap| !swap.candidate_admitted)?;
        let (result, admitted) = publish();
        swap.candidate_admitted = admitted;
        Some(result)
    }
}

impl SupervisedModule {
    /// Blue/green restart: see `SupervisorCommand::Swap` and the
    /// `supervisor_swap` module. Returns once the swap has cut over (the old
    /// process then drains in the background of the supervise loop), has been
    /// superseded by an approved configuration restart, or has failed. Both
    /// supersession and failure leave the old process serving until replacement.
    pub async fn swap(
        &self,
        ready_timeout: Option<Duration>,
    ) -> Result<SwapOutcome, SuperviseError> {
        self.swap_requested(ready_timeout, None).await
    }

    pub(crate) async fn swap_requested(
        &self,
        ready_timeout: Option<Duration>,
        requester: Option<Principal>,
    ) -> Result<SwapOutcome, SuperviseError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(SupervisorCommand::Swap {
                requester,
                ready_timeout,
                reply: reply_tx,
            })
            .await
            .map_err(|_| SuperviseError::CommandClosed {
                module_id: self.inner.module_id.clone(),
            })?;
        reply_rx.await.map_err(|_| SuperviseError::CommandClosed {
            module_id: self.inner.module_id.clone(),
        })?
    }
}
