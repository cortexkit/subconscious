use super::*;

/// How long a restart waits per pipe for queued capture to finish.
///
/// A reader and writer still running at the bound are left running. Stderr
/// stays in the exited process's section of the ring. Its tail reports
/// `Incomplete` until EOF has reached the writer, and stays incomplete if
/// backpressure forced capture to drop lines. Late output from either pipe
/// can reach the capture file after the successor starts.
const STDERR_PUMP_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);

impl SupervisedChild {
    pub(super) fn id(&self) -> Option<u32> {
        Some(self.pid)
    }

    pub(super) fn process_identity(&self) -> Option<ProcessIdentity> {
        self.process_identity
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let result = self.child.try_wait();
        if !matches!(result, Ok(None)) {
            self.retire_registration();
            #[cfg(any(unix, windows))]
            self.registration_channel.take();
            if let Some(binding) = &self._program_binding {
                binding.retire();
            }
        }
        result
    }

    pub(super) async fn wait(&mut self) -> io::Result<ExitStatus> {
        #[cfg(target_os = "macos")]
        self.confirm_privacy_exec().await;
        // The roster entry is NOT released here. A daemon shutdown waits for the
        // roster to empty and then exits the process, so releasing at the reap
        // let it exit before the exit handler wrote this child's terminal record
        // (the stderr drain and snapshot update sit in between), and the
        // shutdown's own `daemon_shutdown` record was intermittently lost. The
        // caller releases it after recording the exit (`release_roster`), and
        // dropping the handle releases it too.
        let result = self.child.wait().await;
        self.retire_registration();
        #[cfg(any(unix, windows))]
        self.registration_channel.take();
        // All plain and swap waits pass here. Revoke the reaped child's nonce
        // before cgroup cleanup, stderr draining or exit observation can await.
        if let Some(binding) = &self._program_binding {
            binding.retire();
        }
        #[cfg(all(test, target_os = "linux"))]
        if let Some(binding) = &self._program_binding {
            binding.pause_after_wait_for_test().await;
        }
        result
    }

    pub(super) async fn finish_exit(&mut self, carry: bool) -> io::Result<()> {
        self.retire_registration();
        // Idempotent fallback for cleanup paths that did not call wait.
        if let Some(binding) = self._program_binding.take() {
            binding.retire();
        }
        if !carry && self.child.container().is_some() {
            self.child.terminate_container().await;
        }
        self.child.reap().await?;
        #[cfg(target_os = "linux")]
        self.cleanup_ordinary_cgroup().await;
        self.release_roster();
        Ok(())
    }
}

impl SupervisedChild {
    /// Releases the leader's roster entry after recording and finalizing its
    /// exit. A carried container retains its separate roster entry and, for a
    /// Unix process group, its unreaped first leader that reserves the group ID.
    pub(super) fn release_roster(&mut self) {
        let _guard = self.roster_guard.take();
        #[cfg(target_os = "linux")]
        if let (Some(unresolved), Some(guard)) = (self.unresolved_cgroup.take(), _guard) {
            unresolved.retain(guard);
        }
    }

    pub(super) fn exit_cleanup_detail(&self, detail: Option<String>) -> Option<String> {
        #[cfg(unix)]
        let detail = if self.late_output_lost {
            Some(match detail {
                Some(detail) => {
                    format!("{detail}; late child output lost: capture pipes did not close")
                }
                None => "late child output lost: capture pipes did not close".into(),
            })
        } else {
            detail
        };
        #[cfg(target_os = "linux")]
        if let Some(unresolved) = &self.unresolved_cgroup {
            return Some(match detail {
                Some(detail) => format!("{detail}; {}", unresolved.detail()),
                None => unresolved.detail(),
            });
        }
        detail
    }
}

impl SupervisedChild {
    /// Kill the child and, where containment is available, its process tree.
    ///
    /// `Child::kill` is `TerminateProcess` scoped to one pid, so a module with a
    /// helper process leaked the helper — the Synapse embedding module's CUDA
    /// worker holds the GPU allocation, so the leak cost VRAM until the next
    /// restart of something else. Terminating the job reaches grandchildren that
    /// a tree walk cannot, including one whose parent has already exited and
    /// been reparented away.
    ///
    /// Best-effort like `request_graceful_stop`: a job failure is logged and the
    /// direct-child kill still decides the outcome, so containment can never
    /// change whether a module is reported as stopped.
    pub(super) fn start_kill(&mut self) -> io::Result<()> {
        self.retire_registration();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if self.child.verified_adoptee().is_some() {
            return self.child.start_kill();
        }
        if let Some(container) = self.child.container() {
            container.hard_kill();
            return self.kill_leader();
        }
        #[cfg(windows)]
        if let Some(job) = &self.job {
            if let Err(error) = job.lock().unwrap_or_else(|p| p.into_inner()).terminate() {
                debug!(
                    error = %error,
                    "job termination failed; the direct-child kill still owns the outcome"
                );
            }
        }
        #[cfg(target_os = "linux")]
        kill_module_cgroup(self.cgroup_placement.as_ref(), &self.module_id);
        self.child.start_kill()
    }

    pub(super) fn kill_leader(&mut self) -> io::Result<()> {
        self.retire_registration();
        self.child.start_kill()
    }

    pub(super) fn start_admission_kill(&mut self) -> io::Result<()> {
        self.retire_registration();
        #[cfg(windows)]
        let result = {
            // Preserve the leader's distinctive status before ending the whole
            // container/job; descendants must not wait for exit finalization.
            let result = self
                .spawned_file_identity
                .as_ref()
                .and_then(|identity| identity.windows_image.as_ref())
                .ok_or_else(|| io::Error::other("admission child handle unavailable"))
                .and_then(|image| {
                    image.process().terminate_with_exit_code(
                        admission_exit::WINDOWS_ADMISSION_KILL_EXIT_CODE as u32,
                    )
                });
            if let Some(container) = self.child.container() {
                container.hard_kill();
            } else if let Some(job) = &self.job {
                if let Err(error) = job.lock().unwrap_or_else(|p| p.into_inner()).terminate() {
                    debug!(%error, "job termination failed after admission leader termination");
                }
            }
            result
        };
        #[cfg(not(windows))]
        let result = self.start_kill().map(|()| true);
        self.admission_kill_sent |= result?;
        Ok(())
    }

    pub(super) async fn drain_stderr(&mut self, module_id: &str) {
        #[cfg(unix)]
        if self.child.plain_group.is_some() {
            self.drain_plain_capture(module_id).await;
            return;
        }
        #[cfg(target_os = "linux")]
        self.cleanup_ordinary_cgroup().await;
        if let Some(pump) = self.stdout_pump.take() {
            settle_stdout_pump(module_id, pump, STDERR_PUMP_DRAIN_TIMEOUT).await;
        }

        let Some(pump) = self.stderr_pump.take() else {
            return;
        };
        settle_stderr_pump(
            module_id,
            &self.stderr_ring,
            pump,
            STDERR_PUMP_DRAIN_TIMEOUT,
        )
        .await;
    }
}

/// Bound the restart's wait. Dropping the completion future does not stop
/// the reader or the capture writer.
pub(super) async fn settle_stdout_pump<E: std::fmt::Display>(
    module_id: &str,
    pump: impl std::future::Future<Output = Result<(), E>>,
    bound: Duration,
) {
    match timeout(bound, pump).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            warn!(module_id, error = %error, "stdout pump ended unexpectedly");
        }
        Err(_) => {
            warn!(
                module_id,
                waited = ?bound,
                "stdout of the exited process has not finished capture; the reader and writer continue without delaying restart"
            );
        }
    }
}

/// Retire an exited process's stderr reader and allow up to `bound` for its
/// output to reach the ring and sink. A late reader or writer keeps going,
/// and the tail reads `Incomplete` until it finishes.
pub(super) async fn settle_stderr_pump<E: std::fmt::Display>(
    module_id: &str,
    ring: &Arc<Mutex<StderrRing>>,
    pump: StderrPump<impl std::future::Future<Output = Result<(), E>>>,
    bound: Duration,
) {
    let lock = || ring.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let StderrPump { task, generation } = pump;
    lock().retire_pump(generation);
    match timeout(bound, task).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            let mut ring = lock();
            ring.mark_incomplete_from(generation, format!("stderr pump ended unexpectedly: {err}"));
            ring.finish_pump(generation);
            warn!(module_id, error = %err, "stderr pump ended before clean EOF");
        }
        Err(_) => {
            lock().mark_pump_late(
                generation,
                format!(
                    "stderr of the exited process had not reached EOF or finished queued capture \
                     {bound:?} after it was retired; retained lines go in that process's section"
                ),
            );
            warn!(
                module_id,
                waited = ?bound,
                "stderr of the exited process has not finished capture; the reader and writer continue without delaying restart"
            );
        }
    }
}

impl SupervisorHandle {
    /// Arm the one-shot marker for the module process that this caller
    /// deliberately initiated severance against. Generic connection teardown
    /// must not call this:
    /// a surviving process would otherwise retain an exemption for a later
    /// genuine crash.
    pub fn record_deliberate_severance(&self, module_id: &str) -> Result<bool, SuperviseError> {
        let Some(module) = self.get(module_id) else {
            return Ok(false);
        };
        let snapshot = lock_snapshot(&module.inner.snapshot)?;
        let Some((pid, start_time)) = snapshot.pid.zip(snapshot.process_start_time) else {
            return Ok(false);
        };
        drop(snapshot);
        module.record_deliberate_severance(ProcessIdentity { pid, start_time })
    }
}

impl SupervisedModule {
    pub(crate) fn record_deliberate_severance(
        &self,
        identity: ProcessIdentity,
    ) -> Result<bool, SuperviseError> {
        let mut snapshot = lock_snapshot(&self.inner.snapshot)?;
        if snapshot.pid != Some(identity.pid)
            || snapshot.process_start_time != Some(identity.start_time)
        {
            return Ok(false);
        }
        snapshot.deliberate_severance = Some(identity);
        Ok(true)
    }
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub(super) async fn on_child_exit(
    spec: &ModuleSpec,
    policy: RestartPolicy,
    registry: &Registry,
    snapshot: &SharedSnapshot,
    terminal_ring: &Arc<Mutex<TerminalRing>>,
    spawn_events: &SpawnEventFeed,
    roster: &ChildRoster,
    exit_report: ExitReport,
    daemon_initiated: bool,
) -> NextAction {
    on_child_exit_with_detail(
        spec,
        policy,
        registry,
        snapshot,
        terminal_ring,
        spawn_events,
        roster,
        exit_report,
        daemon_initiated,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn on_child_exit_with_detail(
    spec: &ModuleSpec,
    policy: RestartPolicy,
    registry: &Registry,
    snapshot: &SharedSnapshot,
    terminal_ring: &Arc<Mutex<TerminalRing>>,
    spawn_events: &SpawnEventFeed,
    roster: &ChildRoster,
    exit_report: ExitReport,
    daemon_initiated: bool,
    detail: Option<String>,
) -> NextAction {
    // Once the daemon has begun shutting down, no exit is a crash to recover
    // from: the module is exiting because the daemon is going away (EOF on its
    // connection, or a service manager signalling the whole cgroup). Record it
    // as such and never schedule a respawn, which would only start a process
    // for the shutdown to end again.
    if roster.is_closed() {
        return on_child_exit_during_daemon_shutdown(
            spec,
            registry,
            snapshot,
            terminal_ring,
            spawn_events,
            exit_report,
            detail,
        )
        .await;
    }
    // Deliberate stops the supervisor asks for (operator stop, disable,
    // restart, reload, swap, a health restart, a drain that runs out of budget)
    // takes the child out of the supervise loop and reaps it in
    // `drain_child_to_state`, and daemon shutdown is handled above. So an exit
    // that reaches this point is either unrequested or a first-HELLO timeout,
    // which is deliberately classified as a crash even after a clean TERM exit.
    //
    // For a subc-wire module a clean exit is still a stop: those modules are
    // written to re-raise SIGTERM, so a stray outside signal already reads as a
    // crash, and exiting 0 is a deliberate choice the module made. A
    // `protocol: "none"` module is a stock program we cannot change, and many
    // of them (nats-server among them) exit 0 on SIGTERM. Treating that as a
    // stop would leave the module down for good after any stray signal, so it
    // goes through the crash path instead: it spends restart budget, respawns
    // with the crash backoff, and ends `failed` when the budget runs out.
    let unrequested_clean_exit_of_protocol_none = exit_report.kind == ExitKind::Clean
        && running_protocol(spec, snapshot) == ModuleProtocol::None;
    match exit_report.kind {
        ExitKind::Clean if !unrequested_clean_exit_of_protocol_none => {
            info!(
                module_id = %spec.module_id,
                exit_code = ?exit_report.code,
                exit_signal = ?exit_report.signal,
                "supervised module exited cleanly"
            );
            if let Err(err) = publish_after_terminal_record(
                &spec.module_id,
                || {
                    record_terminal_with_detail(
                        &spec.module_id,
                        terminal_ring,
                        spawn_events,
                        &exit_report,
                        TerminalDisposition::Stopped,
                        detail,
                    )
                },
                || {
                    update_snapshot(snapshot, Some(&spec.module_id), |state| {
                        state.state = ModuleState::Stopped;
                        clear_current_process_facts(state);
                        state.last_exit = Some(exit_report.clone());
                    })
                },
            ) {
                error!(module_id = %spec.module_id, error = %err, "failed to publish clean module exit");
            }
            let registration_released = match wait_for_registration_release(
                registry,
                &spec.module_id,
                REGISTRY_RELEASE_TIMEOUT,
            )
            .await
            {
                Ok(()) => true,
                Err(err) => {
                    warn!(module_id = %spec.module_id, error = %err, "registration still active after clean exit");
                    false
                }
            };
            NextAction::Stop {
                registration_released,
            }
        }
        ExitKind::Clean | ExitKind::Crash => {
            if unrequested_clean_exit_of_protocol_none {
                warn!(
                    module_id = %spec.module_id,
                    exit_code = ?exit_report.code,
                    exit_signal = ?exit_report.signal,
                    "protocol-none module exited cleanly without a stop request; handling it as a crash"
                );
            } else {
                warn!(
                    module_id = %spec.module_id,
                    exit_code = ?exit_report.code,
                    exit_signal = ?exit_report.signal,
                    "supervised module exited abnormally (crash)"
                );
            }
            let mut restart_schedule = None;
            let mut disposition = TerminalDisposition::Disabled;
            let mut final_state = ModuleState::Disabled;
            // Set only when the budget is what stopped the module, so the
            // terminal record says which limit was hit rather than leaving
            // `failed` to be read as "crashed once, badly".
            let mut disposition_detail = lock_snapshot(snapshot)
                .ok()
                .and_then(|mut state| state.spawn_failure.take());
            if let Some(detail) = detail {
                disposition_detail = Some(
                    disposition_detail
                        .map_or_else(|| detail.clone(), |cause| format!("{cause}; {detail}")),
                );
            }
            let now = Instant::now();
            if let Err(err) = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                if state.enabled {
                    if let Some(schedule) = state.next_crash_restart(&policy, now) {
                        // Route close checks this flag so clients can retry even
                        // before terminal recording publishes Restarting.
                        state.respawn_pending = true;
                        final_state = ModuleState::Restarting;
                        restart_schedule = Some(schedule);
                        disposition = TerminalDisposition::Restarting;
                    } else {
                        final_state = ModuleState::Failed;
                        disposition = TerminalDisposition::Failed;
                        let budget = policy.budget_exhausted_detail();
                        disposition_detail =
                            Some(disposition_detail.take().map_or_else(
                                || budget.clone(),
                                |cause| format!("{cause}; {budget}"),
                            ));
                    }
                } else {
                    disposition = TerminalDisposition::Disabled;
                }
            }) {
                error!(module_id = %spec.module_id, error = %err, "failed to record crashed module exit");
                return NextAction::Stop {
                    registration_released: false,
                };
            }
            if disposition == TerminalDisposition::Failed {
                // The window is in the message, not only in the fields: this line
                // is read in a scrollback where a bare `max_restarts=3` reads as a
                // lifetime cap and sends the operator looking for three crashes
                // that never happened together.
                error!(
                    module_id = %spec.module_id,
                    max_restarts = policy.max_restarts,
                    window_secs = policy.window.as_secs(),
                    "module stopped: {}",
                    policy.budget_exhausted_detail()
                );
            }
            if let Err(err) = publish_after_terminal_record(
                &spec.module_id,
                || {
                    record_terminal_with_detail(
                        &spec.module_id,
                        terminal_ring,
                        spawn_events,
                        &exit_report,
                        disposition,
                        disposition_detail,
                    )
                },
                || {
                    update_snapshot(snapshot, Some(&spec.module_id), |state| {
                        state.state = final_state;
                        clear_current_process_facts(state);
                        if final_state == ModuleState::Failed
                            && daemon_initiated
                            && !roster.is_closed()
                        {
                            state.failed_retry.schedule(&spec.module_id);
                        }
                        state.last_exit = Some(exit_report.clone());
                    })
                },
            ) {
                error!(module_id = %spec.module_id, error = %err, "failed to publish crashed module exit");
            }

            if let Some(schedule) = restart_schedule {
                NextAction::Restart {
                    schedule: Some(schedule),
                }
            } else {
                let registration_released = match wait_for_registration_release(
                    registry,
                    &spec.module_id,
                    REGISTRY_RELEASE_TIMEOUT,
                )
                .await
                {
                    Ok(()) => true,
                    Err(err) => {
                        warn!(module_id = %spec.module_id, error = %err, "registration still active after failed module");
                        false
                    }
                };
                NextAction::Stop {
                    registration_released,
                }
            }
        }
        ExitKind::DeliberateSeverance => {
            warn!(
                module_id = %spec.module_id,
                exit_code = ?exit_report.code,
                exit_signal = ?exit_report.signal,
                "supervised module exited after deliberate connection severance"
            );
            let should_restart = match lock_snapshot(snapshot) {
                Ok(state) => state.enabled,
                Err(err) => {
                    error!(module_id = %spec.module_id, error = %err, "failed to read deliberately severed module state");
                    return NextAction::Stop {
                        registration_released: false,
                    };
                }
            };
            let final_state = if should_restart {
                ModuleState::Restarting
            } else {
                ModuleState::Disabled
            };
            if let Err(err) = publish_after_terminal_record(
                &spec.module_id,
                || {
                    record_terminal_with_detail(
                        &spec.module_id,
                        terminal_ring,
                        spawn_events,
                        &exit_report,
                        terminal_disposition(final_state),
                        detail,
                    )
                },
                || {
                    update_snapshot(snapshot, Some(&spec.module_id), |state| {
                        state.state = final_state;
                        clear_current_process_facts(state);
                        state.last_exit = Some(exit_report.clone());
                        state.lifetime_restarts += 1;
                    })
                },
            ) {
                error!(module_id = %spec.module_id, error = %err, "failed to publish deliberately severed module exit");
                return NextAction::Stop {
                    registration_released: false,
                };
            }

            if should_restart {
                NextAction::Restart { schedule: None }
            } else {
                let registration_released = match wait_for_registration_release(
                    registry,
                    &spec.module_id,
                    REGISTRY_RELEASE_TIMEOUT,
                )
                .await
                {
                    Ok(()) => true,
                    Err(err) => {
                        warn!(module_id = %spec.module_id, error = %err, "registration still active after deliberately severed module exit");
                        false
                    }
                };
                NextAction::Stop {
                    registration_released,
                }
            }
        }
    }
}

async fn on_child_exit_during_daemon_shutdown(
    spec: &ModuleSpec,
    registry: &Registry,
    snapshot: &SharedSnapshot,
    terminal_ring: &Arc<Mutex<TerminalRing>>,
    spawn_events: &SpawnEventFeed,
    exit_report: ExitReport,
    detail: Option<String>,
) -> NextAction {
    info!(
        module_id = %spec.module_id,
        exit_code = ?exit_report.code,
        exit_signal = ?exit_report.signal,
        exit_kind = ?exit_report.kind,
        "supervised module exited during daemon shutdown; not restarting it"
    );
    if let Err(err) = publish_after_terminal_record(
        &spec.module_id,
        || {
            record_terminal_with_detail(
                &spec.module_id,
                terminal_ring,
                spawn_events,
                &exit_report,
                TerminalDisposition::DaemonShutdown,
                detail,
            )
        },
        || {
            update_snapshot(snapshot, Some(&spec.module_id), |state| {
                state.state = ModuleState::Stopped;
                clear_current_process_facts(state);
                state.last_exit = Some(exit_report.clone());
            })
        },
    ) {
        error!(module_id = %spec.module_id, error = %err, "failed to publish module exit during daemon shutdown");
    }
    let registration_released =
        wait_for_registration_release(registry, &spec.module_id, REGISTRY_RELEASE_TIMEOUT)
            .await
            .is_ok();
    NextAction::Stop {
        registration_released,
    }
}

// Keep the status lock out of terminal I/O. Readers may observe the previous
// state while recording is in progress, but never a completed exit without its
// history. Journal errors are logged by the journal and retained in the ring;
// a recording panic still publishes the final state before resuming the unwind.
pub(super) fn publish_after_terminal_record<T>(
    module_id: &str,
    record: impl FnOnce(),
    publish: impl FnOnce() -> T,
) -> T {
    let recorded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(record));
    let published = publish();
    if let Err(panic) = recorded {
        error!(
            module_id,
            "terminal recording panicked; published final state before resuming unwind"
        );
        std::panic::resume_unwind(panic);
    }
    published
}

pub(super) fn fail_after_wait_error(
    module_id: &str,
    snapshot: &SharedSnapshot,
    terminal_ring: &Arc<Mutex<TerminalRing>>,
    spawn_events: &SpawnEventFeed,
    detail: String,
    failure: &FailureContext,
) {
    let report = wait_error_exit_report();
    publish_after_terminal_record(
        module_id,
        || {
            record_terminal_with_detail(
                module_id,
                terminal_ring,
                spawn_events,
                &report,
                TerminalDisposition::Failed,
                Some(detail),
            )
        },
        || fail_snapshot(snapshot, Some(module_id), Some(report.clone()), failure),
    );
}

#[cfg(test)]
pub(super) fn record_wait_error_terminal(
    module_id: &str,
    terminal_ring: &Arc<Mutex<TerminalRing>>,
    spawn_events: &SpawnEventFeed,
) {
    record_terminal(
        module_id,
        terminal_ring,
        spawn_events,
        &wait_error_exit_report(),
        TerminalDisposition::Failed,
    );
}

#[cfg(test)]
pub(super) fn record_terminal(
    module_id: &str,
    terminal_ring: &Arc<Mutex<TerminalRing>>,
    spawn_events: &SpawnEventFeed,
    exit_report: &ExitReport,
    disposition: TerminalDisposition,
) {
    record_terminal_with_detail(
        module_id,
        terminal_ring,
        spawn_events,
        exit_report,
        disposition,
        None,
    );
}

/// Copy the ring under its lock; pin and read the journal files without it.
pub(super) fn durable_terminal_history_of(
    terminal_ring: &Mutex<TerminalRing>,
    module_id: &str,
) -> subc_control::TerminalHistory {
    let read = terminal_ring
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .capture_durable_history();
    read.read(module_id)
}

pub(super) fn record_terminal_with_detail(
    module_id: &str,
    terminal_ring: &Arc<Mutex<TerminalRing>>,
    spawn_events: &SpawnEventFeed,
    exit_report: &ExitReport,
    disposition: TerminalDisposition,
    disposition_detail: Option<String>,
) {
    #[cfg(test)]
    terminal_history_tests::before_terminal_record(spawn_events);
    spawn_events.emit_exited(module_id, exit_report.code, exit_report.signal);
    let record = TerminalRecord {
        exit_code: exit_report.code,
        exit_signal: exit_report.signal,
        at_ms: exit_report.at_ms,
        disposition,
        exit_kind: exit_report.kind.into(),
        disposition_detail,
    };
    terminal_ring
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .record_exit(module_id, record);
}

pub(super) fn untrack_if_registration_released(
    process_liveness: &SupervisorProcessLiveness,
    registry: &Registry,
    module_id: &str,
    snapshot: &SharedSnapshot,
) {
    match registry.get_module(module_id) {
        Ok(None) => process_liveness.untrack_if_current(module_id, snapshot),
        Ok(Some(_)) => {}
        Err(err) => {
            warn!(module_id, error = %err, "could not determine whether supervisor liveness can be untracked");
        }
    }
}

pub(super) fn classify_exit(status: &ExitStatus) -> ExitReport {
    ExitReport {
        kind: if status.success() {
            ExitKind::Clean
        } else {
            ExitKind::Crash
        },
        code: status.code(),
        signal: exit_signal(status),
        at_ms: unix_ms_now(),
    }
}

/// The terminal record for a module whose `wait()` call itself errored (e.g. the
/// child was already reaped out-of-band). There is no `ExitStatus` to read a code
/// or signal from -- `None`/`None` is the honest shape, not a guess -- but the
/// disposition still must be `Failed` so the terminal ring is not silently missing
/// an entry, matching what `fail_snapshot` records for this same arm.
pub(super) fn wait_error_exit_report() -> ExitReport {
    ExitReport {
        kind: ExitKind::Crash,
        code: None,
        signal: None,
        at_ms: unix_ms_now(),
    }
}

#[cfg(unix)]
fn exit_signal(status: &ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;

    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: &ExitStatus) -> Option<i32> {
    None
}

pub(super) fn clear_current_process_facts(state: &mut SupervisorSnapshot) {
    state.legacy_registration_spawn = false;
    state.registration_declaration = None;
    state.subtree_target = crate::subtree_resources::Target::Unavailable("not_running");
    state.failed_retry.healthy_since = None;
    state.failed_retry.running_since = None;
    state.waiting_for_previous_pid = None;
    state.waiting_for_previous_detail = None;
    state.process_alive = false;
    state.spawned_protocol = None;
    state.pid = None;
    #[cfg(windows)]
    {
        state.pipe_consumed = None;
    }
    #[cfg(target_os = "macos")]
    {
        state.report_ready = None;
    }
    state.spawned_at_ms = None;
    state.exec_outcome = None;
    state.spawned_from = None;
    state.launch_spec_digest = None;
    state.spawned_file_identity = None;
    state.process_start_time = None;
    state.deliberate_severance = None;
}

#[cfg(test)]
pub(super) fn record_deliberate_severance(
    snapshot: &SharedSnapshot,
    identity: ProcessIdentity,
) -> Result<(), SuperviseError> {
    update_snapshot(snapshot, None, |state| {
        state.deliberate_severance = Some(identity);
    })
}

pub(super) fn apply_deliberate_severance_marker(
    snapshot: &SharedSnapshot,
    exited_identity: Option<ProcessIdentity>,
    mut exit_report: ExitReport,
) -> ExitReport {
    let marker = lock_snapshot(snapshot)
        .ok()
        .and_then(|mut state| state.deliberate_severance.take());
    if marker.is_some() && marker == exited_identity {
        exit_report.kind = ExitKind::DeliberateSeverance;
    }
    exit_report
}

pub(super) fn classify_reaped_child_exit(
    snapshot: &SharedSnapshot,
    child: &SupervisedChild,
    status: &ExitStatus,
) -> ExitReport {
    let _ = update_snapshot(snapshot, None, |state| {
        state.reaped_pid = Some(child.pid);
        state.spawn_failure = child.spawn_failure.clone();
    });
    if matches!(
        child.spawn_failure.as_deref(),
        Some("registration_timeout" | "registration_channel_closed")
    ) {
        registration_failure_exit_report(classify_exit(status))
    } else {
        apply_deliberate_severance_marker(snapshot, child.process_identity(), classify_exit(status))
    }
}

pub(super) fn fail_snapshot(
    snapshot: &SharedSnapshot,
    module_id: Option<&str>,
    last_exit: Option<ExitReport>,
    failure: &FailureContext,
) {
    if let Err(err) = update_snapshot(snapshot, module_id, |state| {
        state.state = ModuleState::Failed;
        state.config_hold_parked = false;
        state.config_hold_daemon_initiated = false;
        state.config_hold_approval = None;
        clear_current_process_facts(state);
        if failure.may_retry() && state.enabled {
            state.failed_retry.schedule(module_id.unwrap_or("unknown"));
        } else if failure.shutdown.is_set() {
            state.failed_retry.cancel();
        }
        if let Some(last_exit) = last_exit {
            state.last_exit = Some(last_exit);
        }
    }) {
        error!(error = %err, "failed to mark supervisor state failed");
    }
}
