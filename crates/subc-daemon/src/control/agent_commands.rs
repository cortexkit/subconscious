use super::*;
use subc_protocol::session::{AgentCommandFinished, AgentCommandRoot};
mod queue;
pub(super) use queue::Queues;

struct DeclarationValidation<'a> {
    fence: &'a crate::resource_totals::CommitFence,
    arrival: crate::resource_totals::DeclarationArrival,
    defer: Option<&'static str>,
}

// Retaining pending roots changes accounting state just like a final commit, so
// both operations must use the authority carried by the same registration.
enum DeclarationLaunch<'a> {
    Channel(ConnectionId, Arc<crate::supervise::RegistrationSpawn>),
    Nonce(&'a str, &'a str),
}

impl DeclarationLaunch<'_> {
    fn with_current<T>(
        &self,
        handler: &ControlHandler,
        launch: &crate::supervise::ResourceLaunch,
        commit: impl FnOnce() -> T,
    ) -> Result<T, crate::supervise::ResourceCommitError> {
        match self {
            Self::Channel(connection, spawn) => {
                handler.supervisor.with_current_channel_resource_launch(
                    &handler.registry,
                    *connection,
                    spawn,
                    launch,
                    commit,
                )
            }
            Self::Nonce(id, nonce) => handler
                .supervisor
                .with_current_resource_launch(id, nonce, launch, commit),
        }
    }
}

pub(super) fn check_batch(
    frame: &Frame,
    started: usize,
    finished: usize,
    known: usize,
) -> Result<Option<Frame>, RouterError> {
    for (field, len, cap) in [
        ("started", started, 256),
        ("finished", finished, 256),
        ("known", known, 1_024),
    ] {
        if len > cap {
            return control_error_frame(
                frame,
                "invalid_request",
                format!("{field} contains more than {cap} roots"),
            )
            .map(Some);
        }
    }
    Ok(None)
}

impl ControlHandler {
    pub(crate) fn with_daemon_version(mut self, daemon_version: String) -> Self {
        self.resource_tracker = Arc::new(crate::resource_totals::Tracker::new(daemon_version));
        self
    }

    pub(crate) fn spawn_resource_sampler(self: &Arc<Self>) {
        crate::resource_totals::spawn(
            self,
            self.supervisor.clone(),
            Arc::clone(&self.resource_tracker),
        );
    }

    /// Hold at point `stage` when a test asked for it: 0 is before the
    /// current-launch check, 1 is between that check and the launch read.
    #[cfg(test)]
    fn pause_agent_commands_for_test(&self, stage: u8) {
        let mut slot = self
            .agent_commands_pause
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if slot.as_ref().is_some_and(|(at, _)| *at == stage) {
            let (_, (reached, release)) = slot.take().unwrap();
            drop(slot);
            let _ = reached.send(());
            let _ = release.recv_timeout(Duration::from_secs(5));
        }
    }

    pub(super) fn handle_agent_commands(
        &self,
        connection_id: ConnectionId,
        frame: Frame,
        started: Vec<AgentCommandRoot>,
        finished: Vec<AgentCommandFinished>,
        known: Option<Vec<AgentCommandRoot>>,
    ) -> Result<Vec<Frame>, RouterError> {
        self.handle_agent_commands_fenced(
            connection_id,
            frame,
            crate::resource_totals::Declaration {
                started,
                finished,
                known,
            },
            DeclarationValidation {
                fence: &crate::resource_totals::CommitFence::default(),
                arrival: self.resource_tracker.declaration_arrival(),
                defer: None,
            },
        )
    }

    fn handle_agent_commands_fenced(
        &self,
        connection_id: ConnectionId,
        frame: Frame,
        batch: crate::resource_totals::Declaration,
        validation: DeclarationValidation<'_>,
    ) -> Result<Vec<Frame>, RouterError> {
        if let Some(error) = check_batch(
            &frame,
            batch.started.len(),
            batch.finished.len(),
            batch.known.as_ref().map_or(0, Vec::len),
        )? {
            return Ok(vec![error]);
        }
        let registration = self
            .registry
            .get_module_by_connection(connection_id)
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?;
        let Some(registration) = registration else {
            return Ok(vec![control_error_frame(
                &frame,
                "not_supervised",
                "agent commands require the module's supervised connection",
            )?]);
        };
        let id = registration.manifest.module_id;
        #[cfg(test)]
        self.pause_agent_commands_for_test(0);
        let nonce = self.supervisor.spawn_launch_nonce_for(&id);
        // A declaration on a registration channel commits through that spawn,
        // which carries no launch nonce; any other connection keeps the nonce.
        let authority = match self.supervisor.registration_spawn(connection_id) {
            Some(spawn) => DeclarationLaunch::Channel(connection_id, spawn),
            None => DeclarationLaunch::Nonce(&id, nonce.as_deref().unwrap_or_default()),
        };
        let authorized =
            match self.connection_is_current_launch(&id, connection_id, nonce.as_deref()) {
                crate::scopes::LaunchStanding::Current => true,
                crate::scopes::LaunchStanding::NotCurrent => false,
                crate::scopes::LaunchStanding::Unsettled => {
                    return Ok(vec![control_error_frame(
                        &frame,
                        "busy",
                        "the module's current launch cannot be confirmed yet; retry",
                    )?]);
                }
            };
        let module = self.supervisor.get(&id).filter(|_| authorized);
        let Some(module) = module else {
            return Ok(vec![control_error_frame(
                &frame,
                "not_supervised",
                "agent commands require the module's current supervised launch",
            )?]);
        };
        #[cfg(test)]
        self.pause_agent_commands_for_test(1);
        let Some(launch) = module.resource_launch() else {
            return Ok(vec![control_error_frame(
                &frame,
                "busy",
                "module launch snapshot is busy; retry",
            )?]);
        };
        let Some(pid) = launch.pid.filter(|_| launch.enabled && launch.alive) else {
            return Ok(vec![control_error_frame(
                &frame,
                "not_supervised",
                "declaring module has no live process",
            )?]);
        };
        if let Some(reason) = validation.defer {
            if batch.started.is_empty() {
                return Ok(vec![control_error_frame(&frame, "busy", reason)?]);
            }
            // Retention changes only bounded declaration metadata. The cancelled
            // validator cannot publish, and a fresh sampler must prove ownership
            // before any of these roots can move CPU into an agent row.
            let retained = authority.with_current(self, &launch, || {
                self.resource_tracker.retain_started(
                    &id,
                    launch.generation(),
                    pid,
                    &batch.started,
                    validation.arrival,
                    validation.fence,
                )
            });
            return match retained {
                Err(crate::supervise::ResourceCommitError::Changed) => Ok(vec![control_error_frame(
                    &frame, "not_supervised", "the declaring module launch changed during retention",
                )?]),
                Ok(Some(retained)) if retained.roots_full => Ok(vec![control_error_frame(
                    &frame, subc_protocol::error_codes::AGENT_ROOTS_FULL,
                    "live agent command root limit reached; retry unaccepted roots after capacity is available",
                )?]),
                Ok(Some(_)) if batch.finished.is_empty() && batch.known.is_none() => {
                    Ok(vec![control_response_body_frame(
                        &frame, &serde_json::json!({}), "pending agent command acknowledgement",
                    )?])
                }
                _ => Ok(vec![control_error_frame(&frame, "busy", reason)?]),
            };
        }
        let Some(_start) = subc_os::start_time(pid).filter(|start| launch.start == Some(*start))
        else {
            return Ok(vec![control_error_frame(
                &frame,
                "not_supervised",
                "declaring module process identity is no longer live",
            )?]);
        };
        let mut receipt = crate::resource_totals::Receipt::default();
        for attempt in 0..4 {
            let prepared = self.resource_tracker.prepare(
                &id,
                launch.generation(),
                pid,
                launch.target.clone(),
                batch.clone(),
                &mut receipt,
            );
            let Some(prepared) = prepared else {
                if attempt < 3 {
                    std::thread::yield_now();
                    continue;
                }
                break;
            };
            let committed = authority.with_current(self, &launch, || {
                self.resource_tracker.commit_fenced(
                    &id,
                    launch.generation(),
                    prepared,
                    validation.fence,
                )
            });
            let rejected = match committed {
                Ok(rejected) => rejected,
                Err(crate::supervise::ResourceCommitError::Busy) => {
                    if attempt < 3 {
                        std::thread::yield_now();
                        continue;
                    }
                    break;
                }
                Err(crate::supervise::ResourceCommitError::Changed) => {
                    return Ok(vec![control_error_frame(
                        &frame,
                        "not_supervised",
                        "the declaring module launch changed during validation",
                    )?]);
                }
            };
            if rejected.is_none() && attempt < 3 {
                std::thread::yield_now();
                continue;
            }
            let Some(rejected) = rejected else {
                break;
            };
            let roots_full = rejected.roots_full;
            for root in rejected.rejected {
                tracing::warn!(
                    module_id = id,
                    pid = root.pid,
                    start_time_ms = root.start_time_ms,
                    "agent command root was not admitted"
                );
            }
            if roots_full {
                return Ok(vec![control_error_frame(&frame, subc_protocol::error_codes::AGENT_ROOTS_FULL,
                    "live agent command root limit reached; retry unaccepted roots after capacity is available")?]);
            }
            // A successful reply acknowledges valid starts or starts retained
            // pending a complete ownership snapshot. Transient unreadability
            // must not silently discard a declaration that callers won't retry.
            return Ok(vec![control_response_body_frame(
                &frame,
                &serde_json::json!({}),
                "agent command acknowledgement",
            )?]);
        }
        self.handle_agent_commands_fenced(
            connection_id,
            frame,
            batch,
            DeclarationValidation {
                defer: Some("resource state is busy after revalidation; retry uncompleted finished and known entries"),
                ..validation
            },
        )
    }
}
