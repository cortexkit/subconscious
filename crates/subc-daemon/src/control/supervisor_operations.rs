use super::*;

pub(super) const DEFAULT_HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn reload_verdict(
    configured: &Path,
    spawned_from: Option<&Path>,
    image: subc_control::RunningImageAgreement,
) -> PendingReloadVerdict {
    let path = match spawned_from {
        Some(spawned_from) if reload_paths_match(configured, spawned_from) => {
            ReloadPathAgreement::Match
        }
        Some(spawned_from) => ReloadPathAgreement::Mismatch {
            configured: configured.to_path_buf(),
            spawned_from: spawned_from.to_path_buf(),
        },
        None => ReloadPathAgreement::Unavailable {
            reason: if matches!(
                image,
                subc_control::RunningImageAgreement::Unavailable {
                    reason: subc_control::RunningImageUnavailableReason::NotRunning
                }
            ) {
                ReloadPathUnavailableReason::NotRunning
            } else {
                ReloadPathUnavailableReason::SpawnedPathUnavailable
            },
        },
    };
    PendingReloadVerdict::new(path, image)
}

pub(super) fn launch_spec_agreement(
    configured: &crate::supervise::ModuleSpec,
    running: bool,
    spawned: Option<&crate::supervise::LaunchSpecDigest>,
) -> subc_control::LaunchSpecAgreement {
    use subc_control::{LaunchSpecAgreement as Agreement, LaunchSpecUnavailableReason as Reason};
    if !running {
        return Agreement::Unavailable {
            reason: Reason::NotRunning,
        };
    }
    match spawned {
        None => Agreement::Unavailable {
            reason: Reason::SpawnedSpecUnavailable,
        },
        Some(digest) if *digest == crate::supervise::LaunchSpecDigest::from_spec(configured) => {
            Agreement::Match
        }
        Some(_) => Agreement::Mismatch,
    }
}

fn reload_paths_match(configured: &Path, spawned_from: &Path) -> bool {
    if configured == spawned_from {
        return true;
    }
    #[cfg(windows)]
    {
        // Windows spawn records the resolved verbatim path (\\?\...), while
        // configuration keeps its original spelling. Resolve both with the OS
        // instead of stripping prefixes or folding case, which could conflate
        // distinct names in case-sensitive directories. Different resolved
        // paths still differ even if their files have identical content.
        match (
            std::fs::canonicalize(configured),
            std::fs::canonicalize(spawned_from),
        ) {
            (Ok(configured), Ok(spawned_from)) => configured == spawned_from,
            _ => false,
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[derive(Clone)]
pub(super) struct DaemonProvenanceFacts {
    pub(super) build: DaemonBuildProvenance,
    pub(super) pid: Option<u32>,
    pub(super) started_at_ms: Option<u64>,
    pub(super) start_clock: Option<crate::clock::StartClock>,
    pub(super) executable_path: Option<PathBuf>,
    pub(super) executable_identity: Option<SpawnedFileIdentity>,
    pub(super) process_start_time: Option<u64>,
    pub(super) probe: ExecutableIdentityProbe,
}

impl Default for DaemonProvenanceFacts {
    fn default() -> Self {
        Self {
            build: DaemonBuildProvenance {
                build_git_sha: None,
                build_lock_digest: None,
            },
            pid: None,
            started_at_ms: None,
            start_clock: None,
            executable_path: None,
            executable_identity: None,
            process_start_time: None,
            probe: ExecutableIdentityProbe::default(),
        }
    }
}

pub(super) struct ModuleControlRpcGuard {
    forwarding: Arc<ForwardingTable>,
    endpoint: ModuleEndpointId,
    corr: u64,
    armed: bool,
}

impl ModuleControlRpcGuard {
    pub(super) fn new(
        forwarding: Arc<ForwardingTable>,
        endpoint: ModuleEndpointId,
        corr: u64,
    ) -> Self {
        Self {
            forwarding,
            endpoint,
            corr,
            armed: true,
        }
    }

    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ModuleControlRpcGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self
                .forwarding
                .cancel_module_control_rpc(self.endpoint, self.corr);
        }
    }
}

impl ControlHandler {
    #[cfg(test)]
    pub(crate) fn with_health_probe_timeout(mut self, timeout: Duration) -> Self {
        self.health_probe_timeout = timeout;
        self
    }
}

impl ControlHandler {
    pub fn with_daemon_provenance(
        mut self,
        pid: u32,
        started_at_ms: u64,
        executable_path: Option<PathBuf>,
        build_git_sha: Option<String>,
        build_lock_digest: Option<String>,
    ) -> Self {
        let executable_identity = executable_path.as_deref().and_then(spawned_file_identity);
        let process_start_time = process_start_time(pid);
        self.daemon_provenance = DaemonProvenanceFacts {
            build: DaemonBuildProvenance {
                build_git_sha,
                build_lock_digest,
            },
            pid: Some(pid),
            started_at_ms: Some(started_at_ms),
            start_clock: None,
            executable_path,
            executable_identity,
            process_start_time,
            probe: ExecutableIdentityProbe::default(),
        };
        self
    }

    pub(crate) fn with_daemon_start_clock(mut self, clock: crate::clock::StartClock) -> Self {
        self.daemon_provenance.start_clock = Some(clock);
        self
    }
}

impl ControlHandler {
    #[cfg(test)]
    pub(super) fn with_provenance_probe_result(
        mut self,
        result: subc_control::RunningImageAgreement,
    ) -> Self {
        self.provenance_probe_override = Some(result);
        self
    }
}

impl ControlHandler {
    pub(super) fn log_supervisor_request_received(
        &self,
        ctx: &RouteCtx,
        corr: u64,
        op: &'static str,
        module_id: Option<&str>,
        enabled: Option<bool>,
    ) -> Result<(), RouterError> {
        let caller = self.supervisor_requester(ctx, corr)?;
        let caller = principal_label(&caller);
        // `Option` fields are recorded only when present, so a request with no
        // module id (rescan) or no enabled flag simply omits that field.
        info!(
            target: "control",
            op,
            module_id,
            enabled,
            connection_id = ctx.connection_id.get(),
            caller = %caller,
            "supervisor request received"
        );
        Ok(())
    }

    pub(super) fn supervisor_requester(
        &self,
        ctx: &RouteCtx,
        corr: u64,
    ) -> Result<Principal, RouterError> {
        Ok(self
            .registry
            .get_module_by_connection(ctx.connection_id)
            .map_err(|err| RouterError::backend(0, corr, err.to_string()))?
            .map(|registration| Principal::Reserved {
                module_id: registration.manifest.module_id,
            })
            .unwrap_or(Principal::Direct))
    }
}

impl ControlHandler {
    pub(super) fn handle_supervisor_spawn_snapshot(
        &self,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        let response = ClientControlResponse::SupervisorSpawnSnapshot {
            snapshot: self.supervisor.spawn_snapshot(),
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorSpawnSnapshot",
        )?])
    }

    pub(super) fn handle_supervisor_spawn_subscribe(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        since: Option<SpawnCursor>,
    ) -> Result<Vec<Frame>, RouterError> {
        match self.supervisor.subscribe_spawns(
            ctx.connection_id,
            frame.header.corr,
            response_version(&frame),
            since,
            ctx.egress.clone(),
        ) {
            Ok(()) => Ok(Vec::new()),
            Err(SpawnSubscribeRefusal::ForeignIncarnation { current }) => {
                Ok(vec![control_error_body_frame(
                    &frame,
                    ErrorBody {
                        code: "spawn_cursor_incarnation_mismatch".to_string(),
                        message: "spawn cursor belongs to a different daemon incarnation"
                            .to_string(),
                        detail: Some(serde_json::json!({
                            "current_daemon_incarnation": current
                        })),
                    },
                )?])
            }
            Err(SpawnSubscribeRefusal::TooOld { oldest }) => Ok(vec![control_error_body_frame(
                &frame,
                ErrorBody {
                    code: "spawn_cursor_too_old".to_string(),
                    message: "spawn cursor predates the retained event ring".to_string(),
                    detail: Some(serde_json::json!({
                        "oldest_retained_cursor": oldest
                    })),
                },
            )?]),
            Err(SpawnSubscribeRefusal::Frame(error)) => Err(RouterError::backend(
                0,
                frame.header.corr,
                format!("failed to open supervisor spawn subscription: {error}"),
            )),
        }
    }

    pub(super) async fn handle_supervisor_list(
        &self,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        // Status/configuration snapshots may wait on synchronous supervisor
        // locks. Keep that work off the connection's async runtime thread.
        let handler = self.clone();
        let runtime = tokio::runtime::Handle::current();
        // The blocking pool does not inherit the control request's tracing
        // subscriber. Preserve it so slow snapshot-lock warnings stay visible.
        let subscriber = tracing::dispatcher::get_default(Clone::clone);
        let corr = frame.header.corr;
        tokio::task::spawn_blocking(move || {
            runtime.block_on(
                list_diagnostics::scope(handler.handle_supervisor_list_inner(frame))
                    .with_subscriber(subscriber),
            )
        })
        .await
        .map_err(|error| RouterError::backend(0, corr, error.to_string()))?
    }

    pub(super) async fn handle_supervisor_list_inner(
        &self,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        use crate::config_gate::runtime::ApprovedDaemonView;
        let publication = self
            .config_runtime
            .as_ref()
            .and_then(|owner| owner.polled_snapshot().ok());
        let approved = publication
            .as_ref()
            .map(|view| view.approved_daemon.clone())
            .unwrap_or_else(|| {
                if self.config_runtime.is_some() {
                    ApprovedDaemonView::Unavailable
                } else {
                    ApprovedDaemonView::NotStamped
                }
            });
        let generation = self
            .registry
            .generation()
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?;
        let supervised = self.supervisor.list();
        let mut statuses = BTreeMap::new();
        let mut not_running = BTreeSet::new();
        for module in &supervised {
            let status = module.status_for_control("list").map_err(|err| {
                RouterError::backend(
                    0,
                    frame.header.corr,
                    format!("failed to read supervisor status: {err}"),
                )
            })?;
            if !status.enabled || !status.process_alive {
                not_running.insert(module.module_id().to_owned());
            }
            statuses.insert(module.module_id().to_owned(), status);
        }
        // Legacy per-process diagnostics keep their shared optional budget.
        // The cached tree and accounting export below does not spend it.
        let mut process_resources = BTreeMap::new();
        for module in &supervised {
            process_resources.insert(
                module.module_id().to_owned(),
                crate::subtree_resources::read_process(module.clone()).await,
            );
        }
        // Image evidence uses the same optional deadline, but an exhausted
        // budget must not suppress the committed resource view.
        let mut image_readings = BTreeMap::new();
        for module in &supervised {
            image_readings.insert(
                module.module_id().to_owned(),
                module.running_image_agreement().await,
            );
        }
        let readings = self.resource_tracker.list(supervised.clone());
        let mut modules = Vec::new();
        for module in supervised {
            let status = statuses
                .remove(module.module_id())
                .expect("each supervised module has a status snapshot");
            let (configured, _) = module.configuration().map_err(|err| {
                RouterError::backend(
                    0,
                    frame.header.corr,
                    format!("failed to read module configuration: {err}"),
                )
            })?;
            let next_retry_at_ms = module.next_retry_at_ms().map_err(|err| {
                RouterError::backend(
                    0,
                    frame.header.corr,
                    format!("failed to read retry time: {err}"),
                )
            })?;
            let image = image_readings
                .remove(module.module_id())
                .expect("each supervised module has an image snapshot");
            let resources = process_resources.remove(module.module_id());
            let mut verdict =
                reload_verdict(&configured.program, status.spawned_from.as_deref(), image);
            verdict.spec = Some(launch_spec_agreement(
                &configured,
                status.process_alive,
                status.launch_spec_digest.as_ref(),
            ));
            let pending_reload = Some(verdict);
            let pending_entry = match &approved {
                ApprovedDaemonView::Approved { config, .. } => config
                    .modules
                    .iter()
                    .find(|m| m.module_id == module.module_id()),
                _ => None,
            };
            let pending = self
                .pending_module_fields(
                    &module,
                    pending_entry,
                    false,
                    Self::registration_channel_in_force(
                        publication
                            .as_ref()
                            .and_then(|view| view.registration_declaration(module.module_id()))
                            .as_deref(),
                        pending_entry.map(|entry| entry.registration_channel),
                    ),
                    publication.as_deref(),
                )
                .map_err(|err| RouterError::backend(0, frame.header.corr, err))?;
            let script = configured.script.as_ref().map(|entry| {
                let approved = self
                    .config_runtime
                    .as_ref()
                    .and_then(|owner| owner.approved_script(&status.module_id, &entry.path));
                crate::supervise::script::status(entry, approved.as_ref())
            });
            let configured_enabled = match &approved {
                ApprovedDaemonView::Approved {
                    config: approved, ..
                } => approved
                    .modules
                    .iter()
                    .find(|module| module.module_id == status.module_id)
                    .map(|module| module.enabled),
                ApprovedDaemonView::NotStamped => self
                    .capability_evaluator
                    .configured_enabled(&status.module_id),
                ApprovedDaemonView::Unavailable => None,
            };
            let admission_refused = status
                .admission_refused
                .clone()
                .filter(|_| status.enabled && !status.process_alive)
                .filter(|_| {
                    publication
                        .as_ref()
                        .and_then(|view| view.registration_declaration(&status.module_id))
                        .as_deref()
                        == Some("undeclared")
                });
            // Keep the retired policy field on the wire for one release so
            // existing status consumers still receive the platform policy.
            let mut entry = SupervisorEntry::new(
                status.module_id,
                status.state.to_string(),
                status.enabled,
                status.live,
                status.health.status,
            )
            .with_configured_enabled(configured_enabled)
            .with_script(script)
            .with_admission_refused(admission_refused)
            .with_next_retry_at_ms(next_retry_at_ms)
            .with_launch_nonce_env(Some(!cfg!(unix)))
            .with_launch_nonce_source(status.launch_nonce_source)
            .with_waiting_for_previous_pid(status.waiting_for_previous_pid)
            .with_protocol(status.protocol)
            .with_pending_reload(pending_reload)
            .with_last_probe_ms(status.health.last_probe_ms)
            .with_last_exit_code(status.last_exit.as_ref().and_then(|e| e.code))
            .with_last_exit_signal(status.last_exit.as_ref().and_then(|e| e.signal))
            .with_last_exit_ms(status.last_exit.as_ref().map(|e| e.at_ms))
            .with_last_exit_kind(status.last_exit.as_ref().map(|e| e.kind.into()))
            .with_restart_count(Some(status.restart_count))
            .with_max_restarts(Some(status.max_restarts))
            .with_lifetime_restarts(Some(status.lifetime_restarts))
            .with_spawn_generation(Some(status.spawn_generation))
            .with_restart_window_secs(Some(status.restart_window.as_secs()))
            .with_drain_timeout_ms(Some(status.drain_timeout.as_millis() as u64))
            .with_restart_backoff_ms(Some(status.restart_backoff.as_millis() as u64))
            .with_restart_max_backoff_ms(Some(status.restart_max_backoff.as_millis() as u64))
            .with_resources(resources);
            entry.pending = pending;
            modules.push(entry);
        }
        for module in &mut modules {
            if let Some((tree, hour)) = readings.modules.get(&module.module_id) {
                module.tree = tree.clone();
                module.cpu_last_hour = hour.clone();
            }
            module.tree_unavailable = readings
                .reasons
                .get(&module.module_id)
                .map(|reason| (*reason).to_owned());
            if module.tree.is_none() {
                module.tree_unavailable = Some(
                    if not_running.contains(&module.module_id) {
                        "not_running"
                    } else if let Some(reason) = readings.reasons.get(&module.module_id) {
                        reason
                    } else {
                        "not_yet_measured"
                    }
                    .to_owned(),
                );
            }
        }
        let response = ClientControlResponse::SupervisorList {
            generation,
            modules,
            daemon: readings.daemon,
            agent_commands: readings.agent_commands,
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorList",
        )?])
    }

    pub(super) fn handle_supervisor_stderr_tail(
        &self,
        frame: Frame,
        module_id: String,
        max_lines: Option<u32>,
        max_bytes: Option<u32>,
    ) -> Result<Vec<Frame>, RouterError> {
        let Some(module) = self.supervisor.get(&module_id) else {
            return Ok(vec![control_error_frame(
                &frame,
                "unknown_module",
                format!("module_id '{module_id}' is not supervised"),
            )?]);
        };

        let snapshot = module.stderr_tail(
            max_lines.map(|value| value as usize),
            max_bytes.map(|value| value as usize),
        );

        let response = ClientControlResponse::SupervisorStderrTail {
            module_id,
            tail: StderrTail {
                capture: match snapshot.capture {
                    CaptureState::Captured => StderrCaptureState::Captured,
                    CaptureState::Incomplete { reason } => {
                        StderrCaptureState::Incomplete { reason }
                    }
                    CaptureState::NotCaptured { reason } => {
                        StderrCaptureState::NotCaptured { reason }
                    }
                },
                entries: snapshot
                    .entries
                    .into_iter()
                    .map(|entry| match entry {
                        TailEntry::Line {
                            text,
                            truncated,
                            at_ms,
                        } => StderrTailEntry::Line {
                            text,
                            truncated,
                            at_ms,
                        },
                        TailEntry::ProcessStart => StderrTailEntry::ProcessStart,
                    })
                    .collect(),
                dropped_lines: snapshot.dropped_lines,
            },
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorStderrTail",
        )?])
    }

    pub(super) async fn observe_control_wait<F: std::future::Future>(
        &self,
        ctx: &RouteCtx,
        target: Option<&str>,
        kind: &'static str,
        future: F,
    ) -> F::Output {
        self.counters
            .observe_reader_wait(
                || {
                    let module = self
                        .forwarding
                        .module_id_for_connection(ctx.connection_id)
                        .ok()
                        .flatten();
                    let principal = if module.is_some() { "module" } else { "direct" };
                    (
                        self.resolve_reader_wait_target(kind, target)
                            .or(module)
                            .unwrap_or_else(|| crate::observability::CONTROL_WAIT_BUCKET.into()),
                        principal.into(),
                    )
                },
                kind,
                future,
            )
            .await
    }

    /// Resolve the reader-wait module ID only from supervisor or registry
    /// records; never attribute a wait from the request selector alone.
    pub(crate) fn resolve_reader_wait_target(
        &self,
        op: &str,
        selector: Option<&str>,
    ) -> Option<String> {
        let selector = selector?;
        match op {
            ops::SUPERVISOR_TERMINALS | ops::SUPERVISOR_PROVENANCE => self
                .supervisor
                .get(selector)
                .map(|module| module.module_id().to_owned()),
            ops::SUPERVISOR_HEALTH_PROBE => self
                .registry
                .get_module(selector)
                .ok()
                .flatten()
                .map(|registration| registration.manifest.module_id),
            _ => None,
        }
    }

    pub(super) async fn handle_supervisor_terminals(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        module_id: String,
    ) -> Result<Vec<Frame>, RouterError> {
        let Some(module) = self.supervisor.get(&module_id) else {
            return Ok(vec![control_error_frame(
                &frame,
                "unknown_module",
                format!("module_id '{module_id}' is not supervised"),
            )?]);
        };

        // The journal read runs on a blocking thread: it can be megabytes of
        // file I/O and must not occupy a runtime worker.
        let terminals = self
            .observe_control_wait(
                ctx,
                Some(module.module_id()),
                ops::SUPERVISOR_TERMINALS,
                module.read_durable_terminal_history(),
            )
            .await
            .map_err(|error| {
                RouterError::backend(
                    0,
                    frame.header.corr,
                    format!("failed to read terminal history: {error}"),
                )
            })?;
        let response = ClientControlResponse::SupervisorTerminals {
            module_id,
            terminals,
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorTerminals",
        )?])
    }

    pub(super) fn handle_supervisor_routes(
        &self,
        frame: Frame,
        module_id: Option<String>,
    ) -> Result<Vec<Frame>, RouterError> {
        let modules = self
            .forwarding
            .route_census(module_id.as_deref())
            .map_err(RouterError::Forwarding)?
            .into_iter()
            .map(|(module_id, routes)| SupervisorRouteModule {
                module_id,
                routes: routes
                    .into_iter()
                    .map(|route| SupervisorRoute {
                        consumer: match route.principal {
                            Principal::Reserved { module_id } => {
                                SupervisorRouteConsumer::Reserved { module_id }
                            }
                            Principal::Direct | Principal::Unverified => {
                                SupervisorRouteConsumer::Direct {
                                    connection_id: route.goodbye_target.connection_id.get(),
                                }
                            }
                        },
                        age_ms: Instant::now()
                            .saturating_duration_since(route.bound_at)
                            .as_millis()
                            .try_into()
                            .unwrap_or(u64::MAX),
                        draining: route.draining,
                        drain_reason: route.drain_reason,
                    })
                    .collect(),
            })
            .collect();
        let response = ClientControlResponse::SupervisorRoutes { modules };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorRoutes",
        )?])
    }

    fn running_registration_method(
        &self,
        module: &str,
    ) -> Option<subc_control::RegistrationMethod> {
        let registration = self.registry.get_module(module).ok()??;
        let connection = registration.connection_id;
        let channel = self.supervisor.registration_spawn(connection).is_some();
        let nonce = if channel {
            None
        } else {
            self.hello_launch_nonces
                .lock()
                .ok()?
                .nonce(connection)
                .map(str::to_owned)
        };
        if !channel && nonce.is_none() {
            return None;
        }
        self.supervisor
            .registration_snapshot_is_current_live(
                &self.registry,
                module,
                connection,
                nonce.as_deref(),
            )
            .then_some(if channel {
                subc_control::RegistrationMethod::Channel
            } else {
                subc_control::RegistrationMethod::Nonce
            })
    }

    pub(super) async fn handle_supervisor_provenance(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        module_id: Option<String>,
    ) -> Result<Vec<Frame>, RouterError> {
        let targeted = module_id.is_some();
        let selected = if let Some(module_id) = module_id {
            let Some(module) = self.supervisor.get(&module_id) else {
                return Ok(vec![control_error_frame(
                    &frame,
                    "unknown_module",
                    format!("module_id '{module_id}' is not supervised"),
                )?]);
            };
            vec![module]
        } else {
            self.supervisor.list()
        };

        let target = targeted.then(|| selected[0].module_id());
        let declarations = if let Some(owner) = &self.config_runtime {
            owner
                .on_blocking(|owner| owner.registration_declarations())
                .await
                .ok()
        } else {
            None
        };
        self.observe_control_wait(ctx, target, ops::SUPERVISOR_PROVENANCE, async {
            let mut modules = Vec::with_capacity(selected.len());
            let mut extensions = Vec::with_capacity(selected.len());
            for module in &selected {
                let (status, observed_image) = module
                    .status_and_running_image_agreement()
                    .await
                    .map_err(|err| {
                        RouterError::backend(
                            0,
                            frame.header.corr,
                            format!("failed to read supervisor status: {err}"),
                        )
                    })?;
                let module_declared = self
                    .registry
                    .get_module(&status.module_id)
                    .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?
                    .and_then(|registration| registration.manifest.provenance)
                    .map(|build| ModuleDeclaredProvenance::Reported { build })
                    .unwrap_or(ModuleDeclaredProvenance::Unverifiable);
                #[cfg(test)]
                let running_image = match &self.provenance_probe_override {
                    Some(result) => result.clone(),
                    None => observed_image,
                };
                #[cfg(not(test))]
                let running_image = observed_image;
                let registration = self.running_registration_method(&status.module_id);
                let mut extension = subc_control::SupervisorModuleProvenanceExt::new()
                    .with_registration(registration);
                extension.registration_declaration = declarations
                    .as_ref()
                    .and_then(|entries| entries.get(&status.module_id))
                    .cloned();
                if status.process_alive && status.registration_declaration.is_some() {
                    extension.registration_declaration = status.registration_declaration.clone();
                }
                extensions.push(extension);
                modules.push(SupervisorModuleProvenance {
                    module_id: status.module_id,
                    module_declared,
                    daemon_observed: SupervisorObservedProcess {
                        launch_nonce_source: status.launch_nonce_source,
                        pid: status.pid,
                        spawned_at_ms: status.spawned_at_ms,
                        spawned_from: status.spawned_from,
                        running_image,
                    },
                });
            }
            let daemon = SupervisorDaemonProvenance {
                daemon_build: self.daemon_provenance.build.clone(),
                daemon_observed: DaemonObservedProcess {
                    pid: self.daemon_provenance.pid,
                    started_at_ms: self
                        .daemon_provenance
                        .start_clock
                        .map(|clock| clock.started_at_ms())
                        .or(self.daemon_provenance.started_at_ms),
                    running_image: self
                        .daemon_provenance
                        .probe
                        .observe(
                            self.daemon_provenance.pid,
                            self.daemon_provenance.executable_path.as_deref(),
                            self.daemon_provenance.executable_identity.clone(),
                            self.daemon_provenance.process_start_time,
                        )
                        .await,
                },
            };
            let response = ClientControlResponse::SupervisorProvenance { daemon, modules };
            let mut response = serde_json::to_value(response)
                .map_err(|error| RouterError::backend(0, frame.header.corr, error.to_string()))?;
            for (entry, extension) in response["modules"]
                .as_array_mut()
                .expect("provenance modules are an array")
                .iter_mut()
                .zip(extensions)
            {
                extension.write_to(entry).map_err(|error| {
                    RouterError::backend(0, frame.header.corr, error.to_string())
                })?;
            }
            Ok(vec![control_response_body_frame(
                &frame,
                &response,
                "ClientControlResponse::SupervisorProvenance",
            )?])
        })
        .await
    }

    pub(super) fn handle_supervisor_health(&self, frame: Frame) -> Result<Vec<Frame>, RouterError> {
        self.refresh_capability_requirements();
        let publication = self
            .config_runtime
            .as_ref()
            .and_then(|owner| owner.polled_snapshot().ok());
        let generation = self
            .registry
            .generation()
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?;
        let modules = self
            .supervisor
            .list()
            .into_iter()
            .map(|module| {
                let status = module.status_for_control("health").map_err(|err| {
                    RouterError::backend(
                        0,
                        frame.header.corr,
                        format!("failed to read supervisor health: {err}"),
                    )
                })?;
                let module_id = status.module_id;
                let capability_detail = self
                    .capability_evaluator
                    .required_problem_detail(&module_id);
                let declaration_warning = self.registry.get_module(&module_id).ok().flatten()
                    .filter(|registration| registration.manifest.registration_channel == Some(false))
                    .filter(|_| publication.as_ref().and_then(|view| view.registration_declaration(&module_id))
                        .as_deref().map(|next| if status.process_alive { status.registration_declaration.as_deref().unwrap_or(next) } else { next }) == Some("declared"))
                    .map(|_| format!("{module_id}: HELLO registration_channel is false despite the approved channel declaration"));
                let capability_detail = append_capability_problem_detail(capability_detail, declaration_warning);
                Ok(SupervisorHealthEntry {
                    module_id,
                    status: status.health.status,
                    detail: append_capability_problem_detail(
                        status.health.detail,
                        capability_detail,
                    ),
                    metrics: status.health.metrics,
                    consecutive_failures: status.health.consecutive_failures,
                    late_answer_count: status.health.late_answer_count,
                    last_late_answer_latency_ms: status.health.last_late_answer_latency_ms,
                    last_action: status.health.last_action,
                    last_action_ms: status.health.last_action_ms,
                    last_probe_ms: status.health.last_probe_ms,
                })
            })
            .collect::<Result<Vec<_>, RouterError>>()?;
        let response = ClientControlResponse::SupervisorHealth {
            generation,
            modules,
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorHealth",
        )?])
    }

    pub(super) async fn handle_supervisor_restart(
        &self,
        frame: Frame,
        module_id: String,
        drain_timeout_ms: Option<u64>,
        requester: Principal,
    ) -> Result<Vec<Frame>, RouterError> {
        let operation_lock = self.supervisor.operation_lock();
        let _operation_guard = operation_lock.lock().await;
        let Some(module) = self.supervisor.get(&module_id) else {
            return Ok(vec![control_error_frame(
                &frame,
                "unknown_module",
                format!("module_id '{module_id}' is not supervised"),
            )?]);
        };

        self.route_outages.mark_operator_action(&module_id);
        if let Err(err) = module
            .restart_requested(drain_timeout_ms, Some(requester))
            .await
        {
            self.route_outages
                .operator_action_ended_unrefused(&module_id);
            let (code, message) = match err {
                _ if err.is_config_gate_error() => ("config_gated", err.to_string()),
                crate::supervise::SuperviseError::Disabled { .. } => {
                    ("module_disabled", err.to_string())
                }
                crate::supervise::SuperviseError::SwapInProgress { .. } => {
                    ("swap_in_progress", err.to_string())
                }
                _ => (
                    "target_unavailable",
                    format!("failed to restart module_id '{module_id}': {err}"),
                ),
            };
            return Ok(vec![control_error_frame(&frame, code, message)?]);
        }

        let response = ClientControlResponse::SupervisorAck {
            module_id,
            applied: true,
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorAck",
        )?])
    }

    /// `supervisor.swap`. Answered when the swap has cut over or failed, not
    /// when the old process has finished draining: a caller whose own lane
    /// rides the old process must get its reply before that drain waits on it.
    pub(super) async fn handle_supervisor_swap(
        &self,
        frame: Frame,
        module_id: String,
        ready_timeout_ms: Option<u64>,
        requester: Principal,
    ) -> Result<Vec<Frame>, RouterError> {
        // The daemon-wide operation lock is held only to resolve the handle,
        // not across the swap. The swap can take its whole readiness budget,
        // and `supervisor.set_enabled` (ck module stop) takes the same lock:
        // holding it here would park an operator's stop behind the swap it is
        // meant to abort. A rescan or stop that reaches the module during the
        // swap is served by the swap itself (see `supervise_swap`).
        let module = {
            let operation_lock = self.supervisor.operation_lock();
            let _operation_guard = operation_lock.lock().await;
            self.supervisor.get(&module_id)
        };
        let Some(module) = module else {
            return Ok(vec![control_error_frame(
                &frame,
                "unknown_module",
                format!("module_id '{module_id}' is not supervised"),
            )?]);
        };

        self.route_outages.mark_operator_action(&module_id);
        if let Err(err) = module
            .swap_requested(ready_timeout_ms.map(Duration::from_millis), Some(requester))
            .await
        {
            self.route_outages
                .operator_action_ended_unrefused(&module_id);
            use crate::supervise::SuperviseError;
            let message = err.to_string();
            let error = match err {
                SuperviseError::ConfigHeld { .. } => ErrorBody::new("config_gated", message),
                SuperviseError::Disabled { .. } => ErrorBody::new("module_disabled", message),
                SuperviseError::SwapRefused { reason, .. } => ErrorBody {
                    code: "swap_refused".to_string(),
                    message,
                    detail: Some(serde_json::json!({ "reason": reason.as_str() })),
                },
                SuperviseError::SwapFailed {
                    arm,
                    candidate_exit,
                    ..
                } => ErrorBody {
                    code: "swap_failed".to_string(),
                    message,
                    detail: Some(serde_json::json!({
                        "arm": arm.as_str(),
                        "candidate_exit_code": candidate_exit.as_ref().and_then(|exit| exit.code),
                        "candidate_exit_signal": candidate_exit.as_ref().and_then(|exit| exit.signal),
                    })),
                },
                _ => ErrorBody::new(
                    "target_unavailable",
                    format!("failed to swap module_id '{module_id}': {message}"),
                ),
            };
            return Ok(vec![control_error_body_frame(&frame, error)?]);
        }
        // A completed swap kept the incumbent serving until cutover, so it
        // usually opened no outage; a mark left behind would make the next,
        // unrelated outage read as requested.
        self.route_outages
            .operator_action_ended_unrefused(&module_id);

        let response = ClientControlResponse::SupervisorAck {
            module_id,
            applied: true,
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorAck",
        )?])
    }

    pub(super) async fn handle_supervisor_reload(
        &self,
        frame: Frame,
        module_id: String,
        requester: Principal,
    ) -> Result<Vec<Frame>, RouterError> {
        let operation_lock = self.supervisor.operation_lock();
        let _operation_guard = operation_lock.lock().await;
        let Some(module) = self.supervisor.get(&module_id) else {
            return Ok(vec![control_error_frame(
                &frame,
                "unknown_module",
                format!("module_id '{module_id}' is not supervised"),
            )?]);
        };

        self.route_outages.mark_operator_action(&module_id);
        if let Err(err) = module.reload_requested(Some(requester)).await {
            self.route_outages
                .operator_action_ended_unrefused(&module_id);
            let (code, message) = match err {
                _ if err.is_config_gate_error() => ("config_gated", err.to_string()),
                crate::supervise::SuperviseError::Disabled { .. } => {
                    ("module_disabled", err.to_string())
                }
                crate::supervise::SuperviseError::SwapInProgress { .. } => {
                    ("swap_in_progress", err.to_string())
                }
                _ => (
                    "reload_failed",
                    format!("failed to reload module_id '{module_id}': {err}"),
                ),
            };
            return Ok(vec![control_error_frame(&frame, code, message)?]);
        }

        let response = ClientControlResponse::SupervisorAck {
            module_id,
            applied: true,
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorAck",
        )?])
    }
}

impl ControlHandler {
    pub(super) async fn handle_supervisor_release_reserved(
        &self,
        frame: Frame,
        module_id: String,
    ) -> Result<Vec<Frame>, RouterError> {
        let Some(context) = self.rescan.clone() else {
            return Ok(vec![control_error_frame(
                &frame,
                "release_unavailable",
                "reserved-id release requires a daemon started with a reloadable config path",
            )?]);
        };
        let operation_lock = self.supervisor.operation_lock();
        let _operation_guard = operation_lock.lock().await;
        let approved = if let Some(owner) = &self.config_runtime {
            // Observe the file as a proposal, but decide membership solely from
            // approved bytes. A proposed re-add must not retain a removed id.
            match owner
                .on_blocking(|owner| {
                    owner.observe_file_proposal()?;
                    owner.approved_daemon_configuration()
                })
                .await
            {
                Ok(config) => config,
                Err(error) => return Ok(vec![config_gate::config_error_frame(&frame, error)?]),
            }
        } else {
            None
        };
        let loaded = if let Some(approved) = approved {
            Ok(Some(approved))
        } else {
            crate::daemon_config::load(&context.config_path)
        };
        let loaded = match loaded {
            Ok(Some(config)) => config,
            Ok(None) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_daemon_config",
                    format!(
                        "daemon config not found at {}; refusing to release reserved module_id '{module_id}'",
                        context.config_path.display()
                    ),
                )?])
            }
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_daemon_config",
                    format!("unable to verify reserved-id release against daemon config: {err}"),
                )?])
            }
        };
        if loaded
            .modules
            .iter()
            .any(|configured| configured.module_id == module_id)
        {
            return Ok(vec![control_error_frame(
                &frame,
                "reserved_module_configured",
                format!(
                    "module_id '{module_id}' remains configured; remove its config entry and rescan before releasing its reserved id"
                ),
            )?]);
        }
        if !self.supervisor.release_retained_reserved_gate(&module_id) {
            return Ok(vec![control_error_frame(
                &frame,
                "reserved_gate_not_retained",
                format!(
                    "module_id '{module_id}' has no retired reserved-id gate to release; rescan its removed reserved configuration first"
                ),
            )?]);
        }

        let response = ClientControlResponse::SupervisorAck {
            module_id,
            applied: true,
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorAck",
        )?])
    }
}

impl ControlHandler {
    pub(super) async fn handle_supervisor_set_enabled(
        &self,
        frame: Frame,
        module_id: String,
        enabled: bool,
        requester: Principal,
    ) -> Result<Vec<Frame>, RouterError> {
        let operation_lock = self.supervisor.operation_lock();
        let _operation_guard = operation_lock.lock().await;
        let Some(module) = self.supervisor.get(&module_id) else {
            return Ok(vec![control_error_frame(
                &frame,
                "unknown_module",
                format!("module_id '{module_id}' is not supervised"),
            )?]);
        };

        // Enabling counts as well as disabling: a module an operator starts
        // is refused until it registers, and that wait was asked for.
        self.route_outages.mark_operator_action(&module_id);
        let applied = match module.set_enabled_requested(enabled, Some(requester)).await {
            Ok(applied) => applied,
            Err(err) => {
                self.route_outages
                    .operator_action_ended_unrefused(&module_id);
                return Ok(vec![control_error_frame(
                    &frame,
                    if err.is_config_gate_error() {
                        "config_gated"
                    } else {
                        "target_unavailable"
                    },
                    format!("failed to set module_id '{module_id}' enabled={enabled}: {err}"),
                )?]);
            }
        };
        if !applied {
            // Already in the requested state: nothing was made unavailable,
            // so the mark must not outlive this request.
            self.route_outages
                .operator_action_ended_unrefused(&module_id);
        }

        self.capability_evaluator.wake_deadline_loop();
        self.refresh_capability_requirements();
        let response = ClientControlResponse::SupervisorAck { module_id, applied };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorAck",
        )?])
    }

    pub(super) async fn handle_supervisor_health_probe(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        module_id: String,
    ) -> Result<Vec<Frame>, RouterError> {
        self.refresh_capability_requirements();
        let Some(registration) = self
            .registry
            .get_module(&module_id)
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?
        else {
            return Ok(vec![control_error_frame(
                &frame,
                "unknown_module",
                format!("module_id '{module_id}' is not registered"),
            )?]);
        };

        // This guard's ACCEPT direction is fenced, but only INCIDENTALLY: no test is
        // named for it. Making `module_registration_grants_op` return false
        // unconditionally reddens five tests, and every one is named for something
        // else -- capability relay, probe/bind demultiplexing, supervision-only
        // probing. They exercise a successful advertisement check on the way to their
        // own subject.
        //
        // Real protection, fragile in a specific way: narrowing any of those tests to
        // focus on its stated subject would silently remove coverage nobody knows
        // they are carrying. Recorded here rather than as a sixth test, because the
        // useful fact is WHICH tests hold the guard up -- a new test would add
        // coverage without telling the next person what the existing ones quietly do.
        if !module_registration_grants_op(&registration.control_ops, MODULE_CONTROL_OP_HEALTH_CHECK)
        {
            return Ok(vec![control_error_frame(
                &frame,
                "health_not_advertised",
                format!("module_id '{module_id}' did not advertise health.check"),
            )?]);
        }

        self.observe_control_wait(
            ctx,
            Some(&registration.manifest.module_id),
            ops::SUPERVISOR_HEALTH_PROBE,
            async {
                let deadline = Instant::now() + self.health_probe_timeout;
                let pending = match self.forwarding.begin_module_control_rpc_for(
                    &module_id,
                    MODULE_CONTROL_OP_HEALTH_CHECK,
                    deadline,
                ) {
                    Ok(pending) => pending,
                    Err(err) => {
                        return Ok(vec![control_error_frame(
                            &frame,
                            forwarding_error_code(&err),
                            err.to_string(),
                        )?])
                    }
                };

                let PendingModuleControlRpc {
                    endpoint,
                    module_sink,
                    negotiated_ver,
                    corr: probe_corr,
                    receiver,
                } = pending;
                let mut guard =
                    ModuleControlRpcGuard::new(Arc::clone(&self.forwarding), endpoint, probe_corr);
                let probe_body = serde_json::to_vec(&ModuleControlRequest::HealthCheck {})
                    .map_err(|err| {
                        RouterError::backend(
                            0,
                            frame.header.corr,
                            format!("failed to encode health.check request: {err}"),
                        )
                    })?;
                let probe_frame = Frame::build_with_version(
                    negotiated_ver,
                    FrameType::Request,
                    control_flags(),
                    0,
                    0,
                    probe_corr,
                    probe_body,
                )
                .map_err(RouterError::FrameBuild)?;

                // Queue admission and the reply share one deadline, so a module that
                // stops reading cannot park the requesting connection's reader forever.
                match timeout_at(deadline, module_sink.send(probe_frame)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        return Ok(vec![control_error_frame(
                            &frame,
                            "target_unavailable",
                            err.to_string(),
                        )?]);
                    }
                    Err(_) => {
                        return Ok(vec![control_error_frame(
                            &frame,
                            "module_timeout",
                            format!(
                                "module_id '{module_id}' did not answer health.check within {:?}",
                                self.health_probe_timeout
                            ),
                        )?]);
                    }
                }

                match timeout_at(deadline, receiver).await {
                    Ok(Ok(ModuleControlRpcOutcome::RawResponse(_))) => {
                        Ok(vec![control_error_frame(
                            &frame,
                            "invalid_control_body",
                            "health.check returned an unexpected config response",
                        )?])
                    }
                    Ok(Ok(ModuleControlRpcOutcome::Response(response))) => {
                        guard.disarm();
                        let Some(report) = response.health_report() else {
                            return Ok(vec![control_error_frame(
                                &frame,
                                "invalid_control_body",
                                "health.check RPC returned a non-health response",
                            )?]);
                        };
                        // Metrics go out whole here. The supervisor's cached snapshot
                        // caps this blob (see truncate_health_metrics), and this path
                        // exists precisely to answer without that cap -- so applying it
                        // here would leave no way to see what the cached view drops.
                        let HealthReport {
                            status,
                            detail,
                            metrics,
                        } = report;
                        let capability_detail = self
                            .capability_evaluator
                            .required_problem_detail(&module_id);
                        let response = ClientControlResponse::SupervisorHealthProbe {
                            module_id,
                            status,
                            detail: append_capability_problem_detail(detail, capability_detail),
                            metrics,
                        };
                        Ok(vec![control_response_body_frame(
                            &frame,
                            &response,
                            "ClientControlResponse::SupervisorHealthProbe",
                        )?])
                    }
                    Ok(Ok(ModuleControlRpcOutcome::Rejected(body))) => {
                        guard.disarm();
                        // The module refused the health probe with its own body, so
                        // the refusal is relayed without the daemon-origin flag
                        // (bit 6): a client must not read it as the daemon's own
                        // refusal, which it may treat as never forwarded.
                        Ok(vec![control_relayed_error_body_frame(&frame, body)?])
                    }
                    Ok(Ok(ModuleControlRpcOutcome::ModuleGone(message))) => {
                        guard.disarm();
                        Ok(vec![control_error_frame(
                            &frame,
                            "target_unavailable",
                            message,
                        )?])
                    }
                    Ok(Ok(ModuleControlRpcOutcome::MalformedResponse(message))) => {
                        guard.disarm();
                        Ok(vec![control_error_frame(
                            &frame,
                            "invalid_control_body",
                            message,
                        )?])
                    }
                    Ok(Ok(ModuleControlRpcOutcome::UnexpectedOp { expected, actual })) => {
                        guard.disarm();
                        Ok(vec![control_error_frame(
                            &frame,
                            "invalid_control_body",
                            format!("expected module-control op '{expected}', got '{actual}'"),
                        )?])
                    }
                    Ok(Ok(ModuleControlRpcOutcome::DeadlineElapsed)) => {
                        guard.disarm();
                        Ok(vec![control_error_frame(
                            &frame,
                            "module_timeout",
                            format!(
                                "module_id '{module_id}' answered health.check after {:?}",
                                self.health_probe_timeout
                            ),
                        )?])
                    }
                    Ok(Err(_)) => Ok(vec![control_error_frame(
                        &frame,
                        "target_unavailable",
                        "health.check waiter was canceled before the module responded",
                    )?]),
                    Err(_) => Ok(vec![control_error_frame(
                        &frame,
                        "module_timeout",
                        format!(
                            "module_id '{module_id}' did not answer health.check within {:?}",
                            self.health_probe_timeout
                        ),
                    )?]),
                }
            },
        )
        .await
    }

    pub(super) fn supervisor_status(
        &self,
        module_id: &str,
        corr: u64,
    ) -> Result<Option<(crate::supervise::ModuleStatus, bool)>, RouterError> {
        self.supervisor
            .get(module_id)
            .map(|module| {
                let warming = module.is_warming_for_control("status").map_err(|err| {
                    RouterError::backend(
                        0,
                        corr,
                        format!(
                            "failed to read supervisor warming state for module_id '{module_id}': {err}"
                        ),
                    )
                })?;
                module.status_for_control("status").map_err(|err| {
                    RouterError::backend(
                        0,
                        corr,
                        format!(
                            "failed to read supervisor status for module_id '{module_id}': {err}"
                        ),
                    )
                }).map(|status| (status, warming))
            })
            .transpose()
    }
}
