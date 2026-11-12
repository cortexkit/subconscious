use super::*;
use subc_protocol::session::{AgentCommandFinished, AgentCommandRoot};
mod queue;
pub(super) use queue::Queues;

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
            started,
            finished,
            known,
            &crate::resource_totals::CommitFence::default(),
        )
    }

    fn handle_agent_commands_fenced(
        &self,
        connection_id: ConnectionId,
        frame: Frame,
        started: Vec<AgentCommandRoot>,
        finished: Vec<AgentCommandFinished>,
        known: Option<Vec<AgentCommandRoot>>,
        fence: &crate::resource_totals::CommitFence,
    ) -> Result<Vec<Frame>, RouterError> {
        if let Some(error) = check_batch(
            &frame,
            started.len(),
            finished.len(),
            known.as_ref().map_or(0, Vec::len),
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
        let nonce = self.supervisor.spawn_launch_nonce_for(&id);
        // A declaration on a registration channel commits through that spawn,
        // which carries no launch nonce; any other connection keeps the nonce.
        let channel = self.supervisor.registration_spawn(connection_id);
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
        let launch = module.resource_launch().ok_or_else(|| {
            RouterError::backend(
                0,
                frame.header.corr,
                "module launch snapshot is busy; retry",
            )
        })?;
        let Some(pid) = launch.pid.filter(|_| launch.enabled && launch.alive) else {
            return Ok(vec![control_error_frame(
                &frame,
                "not_supervised",
                "declaring module has no live process",
            )?]);
        };
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
                crate::resource_totals::Declaration {
                    started: started.clone(),
                    finished: finished.clone(),
                    known: known.clone(),
                },
                &mut receipt,
            );
            let Some(prepared) = prepared else {
                if attempt < 3 {
                    std::thread::yield_now();
                    continue;
                }
                return Err(RouterError::backend(
                    0,
                    frame.header.corr,
                    "process ownership or membership is unknown; retry",
                ));
            };
            let commit = || {
                self.resource_tracker
                    .commit_fenced(&id, launch.generation(), prepared, fence)
            };
            let committed = match &channel {
                Some(spawn) => self.supervisor.with_current_channel_resource_launch(
                    &self.registry,
                    connection_id,
                    spawn,
                    &launch,
                    commit,
                ),
                None => self.supervisor.with_current_resource_launch(
                    &id,
                    nonce.as_deref().unwrap_or_default(),
                    &launch,
                    commit,
                ),
            };
            let rejected = match committed {
                Ok(rejected) => rejected,
                Err(crate::supervise::ResourceCommitError::Busy) => {
                    if attempt < 3 {
                        std::thread::yield_now();
                        continue;
                    }
                    return Ok(vec![control_error_frame(
                        &frame,
                        "busy",
                        "module launch commit is busy; retry",
                    )?]);
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
                return Ok(vec![control_error_frame(
                    &frame,
                    "busy",
                    "resource state is busy after revalidation; retry",
                )?]);
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
            return Ok(vec![control_response_body_frame(
                &frame,
                &serde_json::json!({}),
                "agent command acknowledgement",
            )?]);
        }
        Ok(vec![control_error_frame(
            &frame,
            "busy",
            "resource accounting could not commit; retry",
        )?])
    }
}
