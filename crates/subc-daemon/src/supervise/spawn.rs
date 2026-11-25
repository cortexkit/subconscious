use super::*;

/// Command-line flag used by supervised modules to find subc.
///
/// subc launches module-mode children as `<module> --subc <connection-file-path>`.
/// The path points at the TCP+key connection file; it is not an ambient signal and
/// is never inherited by standalone children.
pub const SUBC_ARG: &str = "--subc";

// Child admission durably records identity before exec. Disk pressure can make
// fsync slow without implying a failed child start, so allow a generous budget.
#[cfg(any(unix, feature = "test-support"))]
pub(super) const ADMISSION_TIMEOUT: Duration = Duration::from_secs(30);

/// Daemon-side child-admission budget, for integration-test RPC deadlines.
#[cfg(feature = "test-support")]
pub const CHILD_ADMISSION_TIMEOUT: Duration = ADMISSION_TIMEOUT;

#[cfg(unix)]
pub(super) type SpawnRegistrationChannel = std::os::unix::net::UnixStream;
#[cfg(windows)]
pub(super) type SpawnRegistrationChannel = subc_os::registration_channel::RegistrationPipe;
#[cfg(test)]
pub(super) type CapturedRegistrationChannels =
    Arc<Mutex<Option<Vec<(Arc<RegistrationSpawn>, CapturedRegistrationEndpoint)>>>>;
#[cfg(test)]
pub(super) struct CapturedRegistrationEndpoint {
    pub(super) _channel: SpawnRegistrationChannel,
}
#[cfg(test)]
impl fmt::Debug for CapturedRegistrationEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CapturedRegistrationEndpoint")
    }
}

/// The installed companion that evaluates a script module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScriptRuntime {
    Bun,
    Python,
}

impl ScriptRuntime {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bun => "bun",
            Self::Python => "python",
        }
    }
}

/// Script content and the runtime approved to evaluate it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScriptEntry {
    pub runtime: ScriptRuntime,
    pub path: PathBuf,
}

impl ScriptEntry {
    pub fn new(runtime: ScriptRuntime, path: PathBuf) -> Self {
        Self { runtime, path }
    }
}

/// How to launch one singleton module process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleSpec {
    pub module_id: String,
    pub program: PathBuf,
    /// Bundled script content. The program is always the installed runtime runner.
    pub script: Option<ScriptEntry>,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// When true this is a reserved module: each spawn gets a fresh one-time launch
    /// nonce that the child must echo in its HELLO, so only the daemon-spawned
    /// process can register this module_id (a security-boundary module like the
    /// credential vault must not be impersonable while it is down/restarting).
    pub reserved: bool,
    /// Module-id prefixes this supervised module owns for reserved HELLO checks.
    /// Prefixes come from daemon config and must end in `:` before they reach the
    /// supervisor; the owner module's current spawn nonce authorizes claims under
    /// each prefix.
    pub reserved_prefixes: Vec<String>,
    /// The wire protocol this module speaks, as DECLARED in daemon config.
    ///
    /// [`ModuleProtocol::None`] changes five things and nothing else: wire health
    /// probing is suppressed (an optional HTTP probe can replace it), teardown sends SIGTERM before waiting,
    /// `route.open` is refused, the spawn passes NO `--subc <path>` argument
    /// and NO launch nonce, and a clean exit the daemon did not request is
    /// restarted as a crash rather than recorded as a stop (see `on_child_exit`:
    /// a stock program exits 0 on a stray SIGTERM, and a stop would leave it
    /// down for good). `SUBC_MODULE_ID` still goes into the environment,
    /// because a process ignores an environment variable it does not read.
    ///
    /// The argument is the part that cannot be "harmless to a process that
    /// ignores it": a stock binary exits on an unknown flag before it listens
    /// (`nats-server`: "flag provided but not defined: -subc"), which is how the
    /// first conformance run against this mode found it. The nonce is withheld
    /// because a process that will never present it gains nothing from holding
    /// it, and a secret in the environment of a process that does not need it is
    /// a leak surface for no benefit.
    pub protocol: ModuleProtocol,
    /// Whether two processes of this module may run at once, which is what a
    /// blue/green swap does for the length of its overlap. Declared in daemon
    /// config because the daemon must be able to answer it while the module is
    /// down, and so a module cannot talk itself into it after registering.
    pub overlap: ModuleOverlap,
    /// Keep module-started helpers in the same container across replacements.
    /// This is a daemon-config policy, fixed for the lifetime of a container;
    /// stopping the module or losing its supervisor still destroys the container.
    pub carry_containment: bool,
}

/// Only the configured launch inputs enter this value; spawned connection paths,
/// nonces and inherited environment never do. Its bytes remain daemon-private.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct LaunchSpecDigest(pub(super) [u8; 32]);

impl fmt::Debug for LaunchSpecDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LaunchSpecDigest([redacted])")
    }
}

impl LaunchSpecDigest {
    pub(crate) fn from_spec(spec: &ModuleSpec) -> Self {
        fn field(hash: &mut Sha256, bytes: &[u8]) {
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
        let mut hash = Sha256::new();
        if let Some(script) = &spec.script {
            field(
                &mut hash,
                match script.runtime {
                    ScriptRuntime::Bun => b"bun-script",
                    ScriptRuntime::Python => b"python-script",
                },
            );
            field(&mut hash, script.path.as_os_str().as_encoded_bytes());
        }
        if let Some(program) = spec.program.to_str() {
            field(&mut hash, b"utf8-program");
            field(&mut hash, program.as_bytes());
        } else {
            // The daemon config is UTF-8, but in-process callers may provide
            // native paths: lossy conversion would collapse distinct programs.
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                field(&mut hash, b"unix-program");
                field(&mut hash, spec.program.as_os_str().as_bytes());
            }
            #[cfg(windows)]
            {
                use std::os::windows::ffi::OsStrExt;
                field(&mut hash, b"windows-program");
                let bytes = spec
                    .program
                    .as_os_str()
                    .encode_wide()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>();
                field(&mut hash, &bytes);
            }
            #[cfg(not(any(unix, windows)))]
            field(&mut hash, spec.program.to_string_lossy().as_bytes());
        }
        hash.update((spec.args.len() as u64).to_le_bytes());
        for arg in &spec.args {
            field(&mut hash, arg.as_bytes());
        }
        let mut env = spec.env.iter().collect::<Vec<_>>();
        env.sort_unstable();
        hash.update((env.len() as u64).to_le_bytes());
        for (key, value) in env {
            field(&mut hash, key.as_bytes());
            field(&mut hash, value.as_bytes());
        }
        field(
            &mut hash,
            match spec.protocol {
                ModuleProtocol::Subc => b"subc",
                ModuleProtocol::None => b"none",
            },
        );
        Self(hash.finalize().into())
    }
}

/// Whether a module tolerates a second process of itself running alongside.
///
/// Most modules are single-writer on their store (a WAL, a capture log, a
/// resident index behind a writer barrier), and two processes on one store
/// corrupt it. So a swap, which overlaps the old and new process by design,
/// is refused unless the module's config opts in with `overlap: "safe"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModuleOverlap {
    /// Never run two processes of this module at once. The default.
    #[default]
    Exclusive,
    /// The module has said a second process of itself is harmless for the
    /// length of a swap.
    ///
    /// Declare it only if a second instance can run for a few seconds without
    /// touching ANY single-writer store: every database, WAL, index, projector
    /// and scheduled job the module owns. A lease on part of that state is not
    /// enough. broca's session lease guards WAL appends while its run index, its
    /// store projector and its archive fold timer (which unlinks live WAL files)
    /// stay single-writer, so broca is exclusive despite holding a lease. The
    /// refusal only fires after this has been decided, so the decision is the
    /// check.
    Safe,
}

impl ModuleOverlap {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exclusive => "exclusive",
            Self::Safe => "safe",
        }
    }
}

/// Environment variable telling a spawned module which case it was started
/// for, before it sends HELLO. Only a swap candidate carries it, as
/// [`SPAWN_ROLE_SWAP_CANDIDATE`]; every other spawn has it removed.
///
/// It chooses a warm-up budget, nothing else: a swap candidate can warm for
/// longer because nobody waits on it, while a plain restart must flip ready
/// quickly because callers see `module_warming` until it does. Absence means
/// plain restart, the safe reading. The daemon trusts nothing about it; the
/// candidate is proven by its launch nonce at HELLO.
pub const SUBC_SPAWN_ROLE_ENV: &str = "SUBC_SPAWN_ROLE";
/// The one value of [`SUBC_SPAWN_ROLE_ENV`] the daemon sets.
pub const SPAWN_ROLE_SWAP_CANDIDATE: &str = "swap_candidate";

/// A roster entry whose monitor and first process have not started yet.
/// Keeping the command receiver here lets discovery serve status immediately
/// while start/stop requests wait until bootstrap has published the connection file.
pub(crate) struct RegisteredConfiguredModule {
    pub(super) spec: ModuleSpec,
    pub(super) runtime: SupervisorRuntimeConfig,
    pub(super) module: SupervisedModule,
    pub(super) commands: mpsc::Receiver<SupervisorCommand>,
}

/// Test-only hook run on the path that takes on a new module, right after its
/// first `spawn_child` returns (the process exists and could already be
/// registering) and before that process is handed to the module's supervise
/// loop and put on the roster. Lets a test observe what a fast child would see
/// in that window without racing a real one.
#[cfg(test)]
#[derive(Clone, Default)]
pub(super) struct AfterFirstSpawnHook(pub(super) Option<AfterFirstSpawnFn>);

#[cfg(test)]
type AfterFirstSpawnFn = Arc<dyn Fn(&str) + Send + Sync>;

#[cfg(test)]
impl fmt::Debug for AfterFirstSpawnHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AfterFirstSpawnHook")
    }
}

#[cfg(test)]
impl AfterFirstSpawnHook {
    fn run(&self, module_id: &str) {
        if let Some(hook) = &self.0 {
            hook(module_id);
        }
    }
}

impl Supervisor {
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn set_admission_hook_for_test(
        &self,
        hook: impl Fn(&str, &'static str, Option<u32>) -> bool + Send + Sync + 'static,
    ) {
        self.child_roster.set_admission_hook_for_test(hook);
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(registry: Arc<Registry>, policy: RestartPolicy) -> Self {
        let supervisor = Self::new(registry, policy);
        #[cfg(target_os = "macos")]
        let supervisor = supervisor.with_privacy_trampoline(test_privacy_trampoline());
        supervisor
    }
}

impl Supervisor {
    /// Spawn `spec.program` and start monitoring it.
    ///
    /// The child is expected to parse `--subc <connection-file-path>`, read the
    /// TCP+key connection file, authenticate to the already-running listener, and
    /// register with channel-0 `HELLO` using `spec.module_id` as its manifest id.
    ///
    /// This synchronous embedder API blocks on launch admission and store I/O.
    /// Do not call it on an async runtime thread; use [`Self::spawn_async`].
    pub fn spawn(&self, spec: ModuleSpec) -> Result<SupervisedModule, SuperviseError> {
        validate_spec(&spec)?;
        self.establish_identity(&spec);

        let runtime = self.runtime_config();
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        let activity_spawn = runtime.begin_activity_spawn(&spec.module_id);
        let registration_watch = watch_registration(&self.registry, &spec);
        let spawned = spawn_child(
            &spec,
            runtime.connection_file_path.as_deref(),
            self.supervisor_handle.as_ref(),
            &runtime.stderr_ring,
            runtime.capture_logs_dir.as_deref(),
            &runtime.child_roster,
            runtime.spawn_delivery(),
            #[cfg(target_os = "linux")]
            runtime.cgroup_placement.as_ref(),
            runtime.register_timeout,
        );
        #[cfg(test)]
        self.test_after_first_spawn.run(&spec.module_id);
        let mut child = match spawned {
            Ok(child) => child,
            Err(SuperviseError::ConfigDeliveryDeferred { .. }) => {
                defer_config_spawn(
                    &spec,
                    &runtime,
                    &snapshot,
                    RespawnKind::Spawn,
                    &runtime.failure_context(runtime.recovery_is_daemon),
                )?;
                return Ok(self.supervised_module(spec, runtime, snapshot, None));
            }
            Err(SuperviseError::ConfigHeld { .. }) => {
                hold_config_spawn(
                    &spec,
                    &runtime,
                    &snapshot,
                    &runtime.failure_context(runtime.recovery_is_daemon),
                )?;
                return Ok(self.supervised_module(spec, runtime, snapshot, None));
            }
            Err(err) => {
                // Unlike the configured paths, a failed `spawn` leaves nothing
                // on the roster, so the module must not stay marked configured.
                self.abandon_unrostered(&spec.module_id);
                return Err(err);
            }
        };
        arm_registration_deadline(&mut child, runtime.register_timeout, registration_watch);
        self.mark_first_process_running(&spec, &runtime, &snapshot, &mut child)?;
        if let Some(spawn) = activity_spawn {
            spawn.complete(&snapshot);
        }

        Ok(self.supervised_module(spec, runtime, snapshot, Some(child)))
    }

    /// Spawn without running configuration integrity or keychain checks on the
    /// runtime thread. Cancellation during preparation cannot create a child.
    pub async fn spawn_async(&self, spec: ModuleSpec) -> Result<SupervisedModule, SuperviseError> {
        validate_spec(&spec)?;
        self.establish_identity(&spec);
        let mut identity = AsyncInitialIdentity {
            supervisor: self,
            module: spec.module_id.clone(),
            committed: false,
        };
        let runtime = self.runtime_config();
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        let activity_spawn = runtime.begin_activity_spawn(&spec.module_id);
        let registration_watch = watch_registration(&self.registry, &spec);
        let spawned = spawn_child_async(&spec, &runtime).await;
        #[cfg(test)]
        self.test_after_first_spawn.run(&spec.module_id);
        let mut child = match spawned {
            Ok(child) => child,
            Err(SuperviseError::ConfigDeliveryDeferred { .. }) => {
                defer_config_spawn(
                    &spec,
                    &runtime,
                    &snapshot,
                    RespawnKind::Spawn,
                    &runtime.failure_context(runtime.recovery_is_daemon),
                )?;
                identity.committed = true;
                return Ok(self.supervised_module(spec, runtime, snapshot, None));
            }
            Err(SuperviseError::ConfigHeld { .. }) => {
                hold_config_spawn(
                    &spec,
                    &runtime,
                    &snapshot,
                    &runtime.failure_context(runtime.recovery_is_daemon),
                )?;
                identity.committed = true;
                return Ok(self.supervised_module(spec, runtime, snapshot, None));
            }
            Err(err) => {
                self.abandon_unrostered(&spec.module_id);
                return Err(err);
            }
        };
        arm_registration_deadline(&mut child, runtime.register_timeout, registration_watch);
        self.mark_first_process_running(&spec, &runtime, &snapshot, &mut child)?;
        if let Some(spawn) = activity_spawn {
            spawn.complete(&snapshot);
        }
        identity.committed = true;
        Ok(self.supervised_module(spec, runtime, snapshot, Some(child)))
    }

    /// Make `spec`'s module count as configured, with its identity gates
    /// (reserved id, reserved prefixes) in place, BEFORE any process of it
    /// exists.
    ///
    /// Every path that takes on a new module calls this before `spawn_child`.
    /// The order is the point: the child can connect, register, sync its
    /// scopes and ask about them as soon as it is spawned, and the module is
    /// only put on the roster after `spawn_child` returns. Were the mark set
    /// with the roster entry, a fast child would see its own owner reported
    /// as not configured, and a scoped `route.open` in that window would be
    /// refused as terminal `scope_not_live` ("will never sync") instead of
    /// retryable `scope_not_synced`.
    fn establish_identity(&self, spec: &ModuleSpec) {
        if let Some(supervisor_handle) = &self.supervisor_handle {
            supervisor_handle.apply_identity_configuration(spec);
            supervisor_handle.mark_configured(&spec.module_id);
        }
    }

    /// Take back [`Self::establish_identity`]'s configured mark when the
    /// module will not be put on the roster after all.
    fn abandon_unrostered(&self, module_id: &str) {
        if let Some(supervisor_handle) = &self.supervisor_handle {
            supervisor_handle.unmark_configured_unless_rostered(module_id);
        }
    }

    /// Record a freshly spawned first process as running. On failure the
    /// module never reaches the roster, so its configured mark is taken back.
    fn mark_first_process_running(
        &self,
        spec: &ModuleSpec,
        runtime: &SupervisorRuntimeConfig,
        snapshot: &SharedSnapshot,
        child: &mut SupervisedChild,
    ) -> Result<(), SuperviseError> {
        if let Err(err) = set_running(snapshot, child, &spec.module_id, &runtime.spawn_events) {
            containment::terminate_blocking(&runtime.child_roster.containment);
            self.abandon_unrostered(&spec.module_id);
            return Err(err);
        }
        self.process_liveness
            .track(spec.module_id.clone(), Arc::clone(snapshot));
        Ok(())
    }

    /// Start supervising a module declared in daemon configuration.
    ///
    /// Unlike [`Self::spawn`], this records disabled modules and immediate spawn
    /// failures in the supervisor handle so operator-facing `supervisor.list`
    /// reflects every configured module while daemon startup continues.
    pub fn supervise_configured(
        &self,
        spec: ModuleSpec,
        enabled: bool,
    ) -> Result<SupervisedModule, SuperviseError> {
        self.supervise_configured_requested(
            spec,
            enabled,
            self.health.clone(),
            None,
            self.restart_policy,
            DEFAULT_REGISTER_TIMEOUT,
            None,
        )
    }

    /// Supervise a configured module with its own health, drain, and crash
    /// budget. The restart policy is per-module because the config file is:
    /// `modules.<id>.restart` resolves to a full policy at parse time, and a
    /// module that is expensive to restart should not be forced onto the same
    /// budget as one that is cheap.
    pub fn supervise_configured_with_health(
        &self,
        spec: ModuleSpec,
        enabled: bool,
        health: HealthConfig,
        drain_timeout_ms: Option<u64>,
        restart_policy: RestartPolicy,
    ) -> Result<SupervisedModule, SuperviseError> {
        self.supervise_configured_requested(
            spec,
            enabled,
            health,
            drain_timeout_ms,
            restart_policy,
            DEFAULT_REGISTER_TIMEOUT,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn supervise_configured_requested(
        &self,
        spec: ModuleSpec,
        enabled: bool,
        health: HealthConfig,
        drain_timeout_ms: Option<u64>,
        restart_policy: RestartPolicy,
        register_timeout: Duration,
        requester: Option<Principal>,
    ) -> Result<SupervisedModule, SuperviseError> {
        let registered = self.register_configured_with_health(
            spec,
            enabled,
            health,
            drain_timeout_ms,
            restart_policy,
            register_timeout,
        )?;
        let daemon_initiated = requester.is_none();
        self.start_registered_configured(registered, requester, daemon_initiated)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn supervise_configured_requested_async(
        &self,
        spec: ModuleSpec,
        enabled: bool,
        health: HealthConfig,
        drain_timeout_ms: Option<u64>,
        restart_policy: RestartPolicy,
        register_timeout: Duration,
        requester: Option<Principal>,
    ) -> Result<SupervisedModule, SuperviseError> {
        let registered = self.register_configured_with_health(
            spec,
            enabled,
            health,
            drain_timeout_ms,
            restart_policy,
            register_timeout,
        )?;
        let daemon_initiated = requester.is_none();
        self.start_registered_configured_async(registered, requester, daemon_initiated)
            .await
    }

    /// Record a configured module as starting or disabled, without launching a
    /// process or running its monitor. Bootstrap registers the entire roster
    /// before publishing discovery, then activates it once children can read
    /// the connection file passed in their `--subc` argument.
    pub(crate) fn register_configured_with_health(
        &self,
        spec: ModuleSpec,
        enabled: bool,
        health: HealthConfig,
        drain_timeout_ms: Option<u64>,
        restart_policy: RestartPolicy,
        register_timeout: Duration,
    ) -> Result<RegisteredConfiguredModule, SuperviseError> {
        validate_spec(&spec)?;
        self.establish_identity(&spec);

        let mut runtime = self.runtime_config();
        runtime.health = health;
        runtime.restart_policy = restart_policy;
        runtime.register_timeout = register_timeout;
        if let Some(ms) = drain_timeout_ms {
            runtime.drain_timeout = Duration::from_millis(ms);
            *runtime
                .effective_drain_timeout
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = runtime.drain_timeout;
        }
        let snapshot = if enabled {
            SupervisorSnapshot::starting()
        } else {
            SupervisorSnapshot::disabled()
        };
        let snapshot = {
            let mut snapshot = snapshot;
            if enabled {
                if let Some((pid, reason)) = runtime.child_roster.previous_instance(&spec.module_id)
                {
                    snapshot.waiting_for_previous_pid = Some(pid);
                    snapshot.waiting_for_previous_detail = Some(reason.waiting_message(pid));
                }
            }
            snapshot
        };
        let (module, commands) =
            self.register_module(&spec, &runtime, Arc::new(Mutex::new(snapshot)));
        Ok(RegisteredConfiguredModule {
            spec,
            runtime,
            module,
            commands,
        })
    }

    /// Activate a previously registered module. Queued control requests are
    /// served by the monitor after the initial configured spawn has settled.
    pub(crate) fn start_registered_configured(
        &self,
        registered: RegisteredConfiguredModule,
        requester: Option<Principal>,
        daemon_initiated: bool,
    ) -> Result<SupervisedModule, SuperviseError> {
        let RegisteredConfiguredModule {
            spec,
            mut runtime,
            module,
            commands,
        } = registered;
        runtime.start_requester = requester;
        runtime.recovery_is_daemon = daemon_initiated;
        let snapshot = &module.inner.snapshot;
        let should_spawn = {
            let state = lock_snapshot(snapshot)?;
            state.enabled && state.waiting_for_previous_pid.is_none()
        };
        let child = if should_spawn {
            let activity_spawn = runtime.begin_activity_spawn(&spec.module_id);
            let registration_watch = watch_registration(&self.registry, &spec);
            let spawned = spawn_child(
                &spec,
                runtime.connection_file_path.as_deref(),
                self.supervisor_handle.as_ref(),
                &runtime.stderr_ring,
                runtime.capture_logs_dir.as_deref(),
                &runtime.child_roster,
                runtime.spawn_delivery(),
                #[cfg(target_os = "linux")]
                runtime.cgroup_placement.as_ref(),
                runtime.register_timeout,
            );
            #[cfg(test)]
            self.test_after_first_spawn.run(&spec.module_id);
            match spawned {
                Ok(mut child) => {
                    arm_registration_deadline(
                        &mut child,
                        runtime.register_timeout,
                        registration_watch,
                    );
                    self.mark_first_process_running(&spec, &runtime, snapshot, &mut child)?;
                    if let Some(spawn) = activity_spawn {
                        spawn.complete(snapshot);
                    }
                    Some(child)
                }
                Err(SuperviseError::ConfigDeliveryDeferred { .. }) => {
                    defer_config_spawn(
                        &spec,
                        &runtime,
                        snapshot,
                        RespawnKind::Spawn,
                        &runtime.failure_context(daemon_initiated),
                    )?;
                    None
                }
                Err(mut err) => {
                    if matches!(err, SuperviseError::ConfigHeld { .. }) {
                        match hold_config_spawn(
                            &spec,
                            &runtime,
                            snapshot,
                            &runtime.failure_context(daemon_initiated),
                        ) {
                            Ok(()) => {
                                self.start_monitor(spec, runtime, &module, None, commands);
                                return Ok(module);
                            }
                            Err(parking_error) => err = parking_error,
                        }
                    }
                    if runtime.health.critical {
                        error!(
                            module_id = %spec.module_id,
                            program = %spec.program.display(),
                            error = %err,
                            "critical configured module failed to spawn; marking failed and alerting"
                        );
                    } else {
                        error!(
                            module_id = %spec.module_id,
                            program = %spec.program.display(),
                            error = %err,
                            "configured module failed to spawn; marking failed and continuing"
                        );
                    }
                    fail_snapshot(
                        snapshot,
                        Some(&spec.module_id),
                        None,
                        &runtime.failure_context(daemon_initiated),
                    );
                    None
                }
            }
        } else {
            None
        };
        self.start_monitor(spec, runtime, &module, child, commands);
        Ok(module)
    }

    pub(crate) async fn start_registered_configured_async(
        &self,
        registered: RegisteredConfiguredModule,
        requester: Option<Principal>,
        daemon_initiated: bool,
    ) -> Result<SupervisedModule, SuperviseError> {
        self.start_registered_with_failure_policy(registered, requester, daemon_initiated, None)
            .await
    }

    pub(crate) async fn start_registered_approved_async(
        &self,
        registered: RegisteredConfiguredModule,
        requester: Principal,
        generation: crate::config_gate::runtime::ApprovalIdentity,
    ) -> Result<SupervisedModule, SuperviseError> {
        self.start_registered_with_failure_policy(
            registered,
            Some(requester),
            false,
            Some(generation),
        )
        .await
    }

    async fn start_registered_with_failure_policy(
        &self,
        registered: RegisteredConfiguredModule,
        requester: Option<Principal>,
        daemon_initiated: bool,
        approved_generation: Option<crate::config_gate::runtime::ApprovalIdentity>,
    ) -> Result<SupervisedModule, SuperviseError> {
        let RegisteredConfiguredModule {
            spec,
            mut runtime,
            module,
            commands,
        } = registered;
        runtime.start_requester = requester;
        runtime.recovery_is_daemon = daemon_initiated;
        let snapshot = &module.inner.snapshot;
        let mut startup_error = None;
        let failure = FailureContext {
            approval: approved_generation.clone(),
            ..runtime.failure_context(daemon_initiated)
        };
        let should_spawn = {
            let state = lock_snapshot(snapshot)?;
            state.enabled && state.waiting_for_previous_pid.is_none()
        };
        let child = if should_spawn {
            let activity_spawn = runtime.begin_activity_spawn(&spec.module_id);
            let registration_watch = watch_registration(&self.registry, &spec);
            let spawned = spawn_child_async(&spec, &runtime).await;
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                runtime.replacement_ticket = None;
            }
            #[cfg(test)]
            self.test_after_first_spawn.run(&spec.module_id);
            match spawned {
                Ok(mut child) => {
                    arm_registration_deadline(
                        &mut child,
                        runtime.register_timeout,
                        registration_watch,
                    );
                    self.mark_first_process_running(&spec, &runtime, snapshot, &mut child)?;
                    if let Some(spawn) = activity_spawn {
                        spawn.complete(snapshot);
                    }
                    Some(child)
                }
                Err(SuperviseError::ConfigDeliveryDeferred { .. }) => {
                    defer_config_spawn(&spec, &runtime, snapshot, RespawnKind::Spawn, &failure)?;
                    None
                }
                Err(mut err) => {
                    if matches!(err, SuperviseError::ConfigHeld { .. }) {
                        match hold_config_spawn(&spec, &runtime, snapshot, &failure) {
                            Ok(()) => {
                                self.start_monitor(spec, runtime, &module, None, commands);
                                return Ok(module);
                            }
                            Err(parking_error) => err = parking_error,
                        }
                    }
                    if runtime.health.critical {
                        error!(module_id = %spec.module_id, program = %spec.program.display(), error = %err,
                            "critical configured module failed to spawn; marking failed and alerting");
                    } else {
                        error!(module_id = %spec.module_id, program = %spec.program.display(), error = %err,
                            "configured module failed to spawn; marking failed and continuing");
                    }
                    fail_snapshot(snapshot, Some(&spec.module_id), None, &failure);
                    if let Some(generation) = approved_generation {
                        let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                            state.approved_start_failure = Some((generation, err.to_string()));
                        });
                        startup_error = Some(err);
                    }
                    None
                }
            }
        } else {
            None
        };
        self.start_monitor(spec, runtime, &module, child, commands);
        // Applying an operator-approved configuration must report a spawn error
        // without removing the failed module or its monitor. Bootstrap instead
        // tolerates failed starts so other configured modules can still run.
        startup_error.map_or(Ok(module), Err)
    }

    pub(super) fn runtime_config(&self) -> SupervisorRuntimeConfig {
        let effective_drain_timeout = Arc::new(Mutex::new(self.drain_timeout));
        SupervisorRuntimeConfig {
            registry: self.registry.clone(),
            config_delivery: self.config_delivery.clone(),
            recovery_is_daemon: true,
            start_requester: None,
            pending_activity_restart: false,
            provenance_probe: self.provenance_probe.clone(),
            scheduled_respawn: Arc::default(),
            deferred_reload_reply: Arc::default(),
            restart_policy: self.restart_policy,
            register_timeout: DEFAULT_REGISTER_TIMEOUT,
            drain_timeout: self.drain_timeout,
            // Shared with this module's roster copy: daemon shutdown waits on
            // each child for the module's own drain budget, as resolved now.
            child_roster: self
                .child_roster
                .for_module(Arc::clone(&effective_drain_timeout)),
            effective_drain_timeout,
            default_drain_timeout: self.drain_timeout,
            health: self.health.clone(),
            host_health: self.host_health.clone(),
            connection_file_path: self.connection_file_path.clone(),
            capture_logs_dir: self.capture_logs_dir.clone(),
            forwarding: self.forwarding.clone(),
            supervisor_handle: self.supervisor_handle.clone(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            replacement_ticket: None,
            stderr_ring: Arc::new(Mutex::new(StderrRing::new(StderrTailConfig::default()))),
            terminal_ring: Arc::new(Mutex::new(
                TerminalRing::new(
                    TerminalRingConfig::default(),
                    self.daemon_start_clock.started_at_ms(),
                )
                .with_start_clock(self.daemon_start_clock)
                .with_journal(self.terminal_journal.clone())
                .with_daemon_shutdown(self.child_roster.shutdown_flag()),
            )),
            spawn_events: self.spawn_events.clone(),
            #[cfg(target_os = "linux")]
            cgroup_placement: self.cgroup_placement.clone(),
            #[cfg(test)]
            test_seed_stale_facts_before_enable_spawn: false,
            #[cfg(test)]
            test_reload_exit_record_gate: None,
        }
    }

    pub(super) fn supervised_module(
        &self,
        spec: ModuleSpec,
        runtime: SupervisorRuntimeConfig,
        snapshot: SharedSnapshot,
        child: Option<SupervisedChild>,
    ) -> SupervisedModule {
        let (module, commands) = self.register_module(&spec, &runtime, snapshot);
        self.start_monitor(spec, runtime, &module, child, commands);
        module
    }

    pub(super) fn register_module(
        &self,
        spec: &ModuleSpec,
        runtime: &SupervisorRuntimeConfig,
        snapshot: SharedSnapshot,
    ) -> (SupervisedModule, mpsc::Receiver<SupervisorCommand>) {
        runtime.host_health.track(spec, &snapshot);
        let configuration = Arc::new(Mutex::new(SupervisedConfiguration {
            spec: spec.clone(),
            enabled: snapshot.lock().unwrap_or_else(|p| p.into_inner()).enabled,
            health: runtime.health.clone(),
            register_timeout: runtime.register_timeout,
            restart_policy: runtime.restart_policy,
        }));
        let stderr_ring = Arc::clone(&runtime.stderr_ring);
        let terminal_ring = Arc::clone(&runtime.terminal_ring);
        let effective_drain_timeout = Arc::clone(&runtime.effective_drain_timeout);
        let (tx, rx) = mpsc::channel(4);
        let module_id = spec.module_id.clone();
        let module = SupervisedModule {
            inner: Arc::new(SupervisedModuleInner {
                module_id: module_id.clone(),
                registry: Arc::clone(&self.registry),
                snapshot,
                configuration,
                stderr_ring,
                terminal_ring,
                commands: tx,
                monitor: Mutex::new(None),
                effective_drain_timeout,
                provenance_probe: self.provenance_probe.clone(),
                retained: runtime.child_roster.retained_view(),
            }),
        };
        // Identity gates and the configured mark must precede the roster entry,
        // so a client never sees a module without its admission policy.
        if let Some(supervisor_handle) = &self.supervisor_handle {
            supervisor_handle.insert(module.clone());
        }
        (module, rx)
    }

    pub(super) fn start_monitor(
        &self,
        spec: ModuleSpec,
        runtime: SupervisorRuntimeConfig,
        module: &SupervisedModule,
        child: Option<SupervisedChild>,
        commands: mpsc::Receiver<SupervisorCommand>,
    ) {
        let monitor = tokio::spawn(supervise_loop(
            spec,
            runtime,
            Arc::clone(&self.registry),
            Arc::clone(&self.process_liveness),
            Arc::clone(&module.inner.snapshot),
            child,
            commands,
        ));
        *module
            .inner
            .monitor
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(monitor);
    }
}

pub(crate) fn validate_spec(spec: &ModuleSpec) -> Result<(), SuperviseError> {
    if spec.carry_containment && spec.overlap == ModuleOverlap::Safe {
        return Err(SuperviseError::InvalidSpec {
            reason: "carry_containment cannot be combined with overlap: safe".to_owned(),
        });
    }
    if spec.module_id.trim().is_empty() {
        return Err(SuperviseError::InvalidSpec {
            reason: "module_id must not be empty".to_string(),
        });
    }

    Ok(())
}

/// The child's environment plan: inherit the parent's, drop ambient `CK_LOG`,
/// then apply the module's configured entries minus daemon-private capture keys.
///
/// Separated from `spawn_child` only so it can be asserted without spawning a
/// process — a duplicate of this logic in a test would pass while the real one
/// drifted, which is the defect class this function exists to avoid.
/// The subc-wire half of a spawn: `--subc <connection file>` and the launch
/// nonce. A `protocol: "none"` module gets neither, because it cannot use
/// either and the argument would stop a stock binary from starting at all.
/// `SUBC_MODULE_ID` is set on every path since an unread variable is inert.
///
/// The plain-spawn form, kept for the tests that assert its plan; spawns go
/// through [`apply_wire_spawn_args_for_admission`].
#[cfg(test)]
pub(super) fn apply_wire_spawn_args(
    command: &mut Command,
    spec: &ModuleSpec,
    connection_file_path: Option<&std::path::Path>,
    handle: Option<&SupervisorHandle>,
) -> Result<Option<NonceHandoff>, SuperviseError> {
    apply_wire_spawn_args_for_role(
        command,
        spec,
        connection_file_path,
        handle,
        SpawnRole::Plain,
    )
}

/// The read end of a spawn's launch-nonce pipe, prepared by
/// [`apply_wire_spawn_args_for_admission`] and installed as the child's descriptor 3
/// by the last pre-exec step, just before `spawn()`. Windows prepares a named
/// server before spawn and authenticates its client after the child exists.
#[cfg(unix)]
type NonceHandoff = subc_os::LaunchNonceHandoff;
#[cfg(windows)]
type NonceHandoff = subc_os::LaunchNoncePipeHandoff;
#[cfg(not(any(unix, windows)))]
type NonceHandoff = std::convert::Infallible;

/// Create the launch nonce that a new module process must present when it
/// registers or opens a consumer connection, including a replacement process
/// started alongside a still-serving module.
///
/// A plain spawn replaces the module's recorded nonce. A swap candidate records
/// a separate candidate token so the still-serving incumbent and its consumers
/// keep their nonce. Both records are installed before the process exists, so
/// the child's initial HELLO registration cannot arrive ahead of its nonce.
///
/// On Unix the nonce is delivered only through a pipe. It is written into
/// a pipe whose read end the child gets as descriptor 3, named by
/// `SUBC_LAUNCH_NONCE_FD=3:<pipe inode>`: unlike the environment, another
/// process of the same user cannot read it with `ps eww`. That handoff is
/// returned rather than installed here. The combined descriptor placement runs
/// after the cgroup callback and placement barrier, so replacing slots 3-5
/// cannot close a descriptor either callback still needs.
/// Windows keeps SUBC_LAUNCH_NONCE as well until every module reads the named pipe.
#[cfg(test)]
fn apply_wire_spawn_args_for_role(
    command: &mut Command,
    spec: &ModuleSpec,
    connection_file_path: Option<&std::path::Path>,
    handle: Option<&SupervisorHandle>,
    role: SpawnRole,
) -> Result<Option<NonceHandoff>, SuperviseError> {
    apply_wire_spawn_args_for_admission(command, spec, connection_file_path, handle, role, None)
}

pub(super) fn apply_wire_spawn_args_for_admission(
    command: &mut Command,
    spec: &ModuleSpec,
    connection_file_path: Option<&std::path::Path>,
    handle: Option<&SupervisorHandle>,
    role: SpawnRole,
    admission: Option<&crate::config_gate::runtime::SpawnAdmission>,
) -> Result<Option<NonceHandoff>, SuperviseError> {
    command.env(SUBC_MODULE_ID_ENV, &spec.module_id);
    // SUBC_LAUNCH_NONCE_FD is removed for every child, `protocol: "none"`
    // included: a daemon started from a module's process tree inherits it,
    // and passing it on would point the child at a descriptor it does not
    // have.
    command.env_remove(subc_os::LAUNCH_NONCE_FD_ENV);
    #[cfg(windows)]
    {
        command.env_remove(subc_os::LAUNCH_NONCE_PIPE_ENV);
        command.env_remove(subc_os::LAUNCH_NONCE_PIPE_FALLBACK_ENV);
    }
    // Remove inherited or configured copies too: withholding must mean absent.
    command.env_remove(SUBC_LAUNCH_NONCE_ENV);
    command.env_remove(subc_os::registration_channel::REGISTRATION_FD_ENV);
    command.env_remove(subc_os::registration_channel::REGISTRATION_PIPE_ENV);
    command.env_remove(subc_os::registration_channel::DAEMON_PID_ENV);
    if role == SpawnRole::Plain {
        if let Some(handle) = handle {
            #[cfg(all(test, unix))]
            {
                let pause = {
                    let mut slot = handle.plain_identity_policy_pause.lock().unwrap();
                    if slot
                        .as_ref()
                        .is_some_and(|(module, _)| module == &spec.module_id)
                    {
                        slot.take().map(|(_, pause)| pause)
                    } else {
                        None
                    }
                };
                if let Some(pause) = pause {
                    pause.reached.send(()).unwrap();
                    pause.release.recv().unwrap();
                }
            }
            // A rescan preserves the running child's nonce and reservation.
            // Replacement spawn preparation applies the new protocol now,
            // before protocol:none can return without minting a nonce.
            handle.apply_identity_policy(spec, spec.protocol);
        }
    }
    if spec.protocol == ModuleProtocol::None {
        return Ok(None);
    }
    if let Some(connection_file_path) = connection_file_path {
        command.arg(SUBC_ARG).arg(connection_file_path);
    }

    if admission.is_some_and(|admission| admission.launch.is_some()) {
        if let Some(handle) = handle {
            match role {
                SpawnRole::Plain => {
                    handle
                        .spawn_nonces
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&spec.module_id);
                    if spec.reserved {
                        handle
                            .reserved_nonces
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .insert(spec.module_id.clone(), None);
                    }
                }
                // Swaps still need their lifecycle record, but the empty value
                // is not a bearer token and is never accepted by nonce HELLO.
                SpawnRole::SwapCandidate => handle.open_swap(&spec.module_id, String::new()),
            }
        }
        return Ok(None);
    }
    // Every subc-wire spawn receives a fresh one-time launch nonce for consumer
    // route.open attestation. Reserved modules additionally use the same nonce
    // for HELLO id-squatting protection. A respawn rotates both records.
    let nonce = generate_launch_nonce()?;
    if let Some(handle) = handle {
        match role {
            SpawnRole::Plain => {
                handle.set_spawn_nonce(&spec.module_id, nonce.clone());
                if spec.reserved {
                    handle.set_reserved_nonce(&spec.module_id, nonce.clone());
                }
            }
            SpawnRole::SwapCandidate => handle.open_swap(&spec.module_id, nonce.clone()),
        }
    }
    #[cfg(unix)]
    let handoff = {
        let handoff =
            subc_os::LaunchNonceHandoff::new(&nonce).map_err(|source| SuperviseError::Spawn {
                program: spec.program.clone(),
                source,
                cgroup_path: None,
            })?;
        command.env(subc_os::LAUNCH_NONCE_FD_ENV, handoff.fd_env_value());
        Some(handoff)
    };
    #[cfg(windows)]
    let handoff = prepare_windows_nonce_pipe(command, spec, &nonce);
    #[cfg(not(any(unix, windows)))]
    let handoff = None;
    // Older modules still read SUBC_LAUNCH_NONCE to register. Keep that copy
    // until the daemon's per-spawn delivery records show that every running
    // Windows module has read its secret from the named pipe.
    #[cfg(not(unix))]
    command.env(SUBC_LAUNCH_NONCE_ENV, nonce);
    Ok(handoff)
}

pub(crate) fn apply_child_env(command: &mut Command, spec: &ModuleSpec) {
    command.env_remove(CK_LOG_ENV);
    // The spawn role is the supervisor's to set, and only on a swap candidate
    // (see `apply_spawn_role`). Removing it here, rather than just not setting
    // it, is what makes it absent on a plain spawn: the daemon's own
    // environment could carry it, and so could a spec built outside daemon
    // config (config refuses it as an `env` key). A module reading it on a
    // plain restart would pick the long swap budget and leave callers waiting.
    command.env_remove(SUBC_SPAWN_ROLE_ENV);
    for (key, value) in &spec.env {
        // cortexkit-log currently exposes retention only as a Rust struct, not
        // environment names. These values are daemon-private sink metadata and
        // must never become a public child-process contract by being inherited.
        if matches!(
            key.as_str(),
            CAPTURE_MAX_FILE_MB_ENV | CAPTURE_KEEP_ENV | CAPTURE_MAX_AGE_DAYS_ENV
        ) || key == SUBC_SPAWN_ROLE_ENV
        {
            continue;
        }
        command.env(key, value);
    }
}

#[cfg(windows)]
pub(super) fn prepare_windows_registration_pipe(
    command: &mut Command,
    spec: &ModuleSpec,
) -> Result<Option<SpawnRegistrationChannel>, SuperviseError> {
    if spec.protocol == ModuleProtocol::None {
        return Ok(None);
    }
    let pipe = subc_os::registration_channel::RegistrationPipe::new().map_err(|source| {
        SuperviseError::Spawn {
            program: spec.program.clone(),
            source,
            cgroup_path: None,
        }
    })?;
    command
        .env(
            subc_os::registration_channel::REGISTRATION_PIPE_ENV,
            pipe.name(),
        )
        .env(
            subc_os::registration_channel::DAEMON_PID_ENV,
            std::process::id().to_string(),
        );
    Ok(Some(pipe))
}

#[cfg(windows)]
fn prepare_windows_nonce_pipe(
    command: &mut Command,
    spec: &ModuleSpec,
    nonce: &str,
) -> Option<NonceHandoff> {
    match subc_os::LaunchNoncePipeHandoff::new(nonce) {
        Ok(handoff) => {
            command.env(subc_os::LAUNCH_NONCE_PIPE_ENV, handoff.name());
            // Let a failed pipe read use SUBC_LAUNCH_NONCE so the process can
            // still register until every module reads the pipe. A reader without
            // this variable set to env returns the pipe error instead of using
            // an inherited or configured environment copy.
            command.env(subc_os::LAUNCH_NONCE_PIPE_FALLBACK_ENV, "env");
            Some(handoff)
        }
        Err(error) => {
            warn!(module_id = %spec.module_id, %error, "could not prepare launch nonce pipe; retaining the environment handoff");
            None
        }
    }
}

#[cfg(windows)]
fn start_windows_nonce_pipe(
    handoff: Option<NonceHandoff>,
    pid: u32,
    spec: &ModuleSpec,
) -> Option<subc_os::LaunchNoncePipeDelivery> {
    // Close an unread pipe after REGISTRY_RELEASE_TIMEOUT (one second), the
    // same maximum wait used for registration during a reload. Older modules
    // may register using SUBC_LAUNCH_NONCE without ever connecting to the pipe.
    handoff.and_then(|handoff| match handoff.serve(pid, REGISTRY_RELEASE_TIMEOUT) {
        Ok(delivery) => Some(delivery),
        Err(error) => {
            warn!(module_id = %spec.module_id, %error, "could not serve launch nonce pipe; retaining the environment handoff");
            None
        }
    })
}

pub(super) fn observed_launch_nonce_source(snapshot: &SupervisorSnapshot) -> Option<String> {
    #[cfg(windows)]
    if snapshot.process_alive && snapshot.spawned_protocol == Some(ModuleProtocol::Subc) {
        return Some(
            if snapshot
                .pipe_consumed
                .as_ref()
                .is_some_and(|consumed| consumed.get().is_some())
            {
                "pipe"
            } else {
                "env"
            }
            .to_owned(),
        );
    }
    let _ = snapshot;
    None
}

/// Which slot a spawn fills: the module's ordinary one, or the candidate slot
/// of a blue/green swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpawnRole {
    Plain,
    SwapCandidate,
}

/// Set the spawn role for a swap candidate. A plain spawn gets nothing here;
/// `apply_child_env` has already removed the variable for every spawn.
pub(super) fn apply_spawn_role(command: &mut Command, role: SpawnRole) {
    if role == SpawnRole::SwapCandidate {
        command.env(SUBC_SPAWN_ROLE_ENV, SPAWN_ROLE_SWAP_CANDIDATE);
    }
}

pub(super) fn previous_instance_error(
    spec: &ModuleSpec,
    pid: u32,
    reason: crate::live_children::RetainedReason,
) -> SuperviseError {
    SuperviseError::Spawn {
        program: spec.program.clone(),
        source: io::Error::new(io::ErrorKind::WouldBlock, reason.waiting_message(pid)),
        cgroup_path: None,
    }
}

pub(super) fn carries_containment(configured: bool, has_container: bool) -> bool {
    configured || has_container
}

pub(super) fn recovery_block_error(
    spec: &ModuleSpec,
    roster: &ChildRoster,
) -> Option<SuperviseError> {
    if !carries_containment(
        spec.carry_containment,
        containment::current(&roster.containment).is_some(),
    ) {
        return None;
    }
    Some(SuperviseError::Spawn {
        program: spec.program.clone(),
        source: io::Error::new(io::ErrorKind::WouldBlock, roster.recovery_block()?),
        cgroup_path: None,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_child(
    spec: &ModuleSpec,
    connection_file_path: Option<&std::path::Path>,
    handle: Option<&SupervisorHandle>,
    ring: &Arc<Mutex<StderrRing>>,
    capture_logs_dir: Option<&std::path::Path>,
    roster: &ChildRoster,
    config_delivery: Option<config_delivery::SpawnDelivery<'_>>,
    #[cfg(target_os = "linux")] cgroup_placement: Option<&subc_cgroup::Placement>,
    register_timeout: Duration,
) -> Result<SupervisedChild, SuperviseError> {
    #[cfg(all(test, unix))]
    if let Some(source) = containment_tests::join_failure(spec, roster) {
        containment::terminate_blocking(&roster.containment);
        return Err(SuperviseError::Spawn {
            program: spec.program.clone(),
            source,
            cgroup_path: None,
        });
    }
    let result = spawn_child_in_slot(
        spec,
        connection_file_path,
        handle,
        ring,
        capture_logs_dir,
        roster,
        config_delivery,
        #[cfg(target_os = "linux")]
        cgroup_placement,
        SpawnRole::Plain,
        false,
        register_timeout,
    );
    if result
        .as_ref()
        .is_err_and(|error| !is_launch_deferral(error))
    {
        if roster.admission_pending(&spec.module_id) {
            containment::terminate_deferred(&roster.containment);
        } else {
            containment::terminate_blocking(&roster.containment);
        }
    }
    result
}

/// A configuration deferral means the launch never started: the module gate
/// or the configuration owner was briefly busy, and every caller schedules
/// the same launch again shortly. A carried container stays for that retry,
/// as it does across a restart backoff; ending it here would kill the
/// module's helpers and give up the group the retry is meant to join.
pub(super) fn is_launch_deferral(error: &SuperviseError) -> bool {
    matches!(error, SuperviseError::ConfigDeliveryDeferred { .. })
}

/// Spawn one process of `spec` into a slot.
///
/// `alternate_slot` picks the process's cgroup label on Linux (see `swap::cgroup_name`).
/// A swap candidate needs a different cgroup from the process it is replacing,
/// which is still alive: in the same cgroup the two would be one kill domain,
/// and killing a failed candidate could take the incumbent with it.
///
/// The stderr capture file is `<module_id>.stderr.log` for every process of
/// the module, whichever slot it is in, because that is the one file
/// `ck module logs` reads. During a swap's overlap both processes append to it;
/// the daemon writes whole lines, so the two interleave by line, which is also
/// the merged view an operator wants while a swap runs.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_child_in_slot(
    spec: &ModuleSpec,
    connection_file_path: Option<&std::path::Path>,
    handle: Option<&SupervisorHandle>,
    ring: &Arc<Mutex<StderrRing>>,
    capture_logs_dir: Option<&std::path::Path>,
    roster: &ChildRoster,
    config_delivery: Option<config_delivery::SpawnDelivery<'_>>,
    #[cfg(target_os = "linux")] cgroup_placement: Option<&subc_cgroup::Placement>,
    role: SpawnRole,
    alternate_slot: bool,
    register_timeout: Duration,
) -> Result<SupervisedChild, SuperviseError> {
    let prepared = prepare_child_in_slot(
        spec,
        connection_file_path,
        handle,
        capture_logs_dir,
        roster,
        config_delivery,
        #[cfg(target_os = "linux")]
        cgroup_placement,
        role,
        alternate_slot,
        false,
    )?;
    let gate = config_delivery
        .map(|delivery| delivery.module_gate(spec))
        .transpose()?;
    let _gate = gate
        .as_ref()
        .map(|gate| {
            gate.try_lock()
                .map_err(|_| SuperviseError::ConfigDeliveryDeferred {
                    module_id: spec.module_id.clone(),
                })
        })
        .transpose()?;
    if let (Some(delivery), Some(admission)) = (config_delivery, &prepared.spawn_admission) {
        delivery.check(spec, admission)?;
    }
    let checked_base = prepared
        .spawn_admission
        .as_ref()
        .and_then(|admission| admission.approved_base())
        .cloned();
    let mut result = launch_child_in_slot(spec, ring, roster, prepared, register_timeout);
    drop(_gate);
    if let (Some(delivery), Err(SuperviseError::ConfigHeld { code, detail, .. })) =
        (config_delivery, &result)
    {
        if let Some(base) = checked_base.as_ref().filter(|_| code == "program_refused") {
            if !delivery.record_refusal(spec, base, detail)? {
                result = Err(spawn_plan_stale(spec));
            }
        }
    }
    result
}

struct AsyncInitialIdentity<'a> {
    supervisor: &'a Supervisor,
    module: String,
    committed: bool,
}
impl Drop for AsyncInitialIdentity<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.supervisor.abandon_unrostered(&self.module);
        }
    }
}

/// Child-free preparation can be abandoned without orphaning a process. After
/// this value returns, the async caller validates its launch witness and creates
/// and publishes the child without another await.
pub(super) struct PreparedChild {
    script_delivery: Option<script::Delivery>,
    registration_spawn: Option<Arc<RegistrationSpawn>>,
    registration_handle: Option<SupervisorHandle>,
    #[cfg(any(unix, windows))]
    registration_channel: Option<SpawnRegistrationChannel>,
    config_context: Option<config_delivery::ConfigDelivery>,
    binding_publication: Option<ProgramBindingPublication>,
    launch_spec: ModuleSpec,
    spawn_admission: Option<crate::config_gate::runtime::SpawnAdmission>,
    pub(super) command: Command,
    carrying: bool,
    carried: Option<Arc<containment::CarriedContainment>>,
    #[cfg(unix)]
    launch_identity: Option<containment::LaunchIdentity>,
    #[cfg(unix)]
    group_target: containment::GroupTarget,
    #[cfg(target_os = "linux")]
    pub(super) cgroup_placement: Option<subc_cgroup::Placement>,
    #[cfg(target_os = "linux")]
    pub(super) cgroup_name: String,
    #[cfg(target_os = "linux")]
    cgroup_cleanup: PreparedCgroup,
    pub(super) cgroup_path: Option<PathBuf>,
    #[cfg(unix)]
    admission_gate: Option<admission::Gate>,
    output_sink: ChildOutputSink,
    #[cfg(target_os = "macos")]
    privacy_exec: Option<PrivacyExec>,
    #[cfg(windows)]
    image_capture: subc_os::ExecutableCapture,
    #[cfg(windows)]
    nonce_handoff: Option<NonceHandoff>,
    spawned_at: Instant,
    config_delivery: Option<crate::config_gate::runtime::DeliverySpawn>,
    delivery_registration: Option<config_delivery::RegistrationGuard>,
    program_binding: Option<SpawnProgramBinding>,
}

#[cfg(target_os = "linux")]
struct PreparedCgroup(Option<(subc_cgroup::Placement, String)>);
#[cfg(target_os = "linux")]
impl Drop for PreparedCgroup {
    fn drop(&mut self) {
        if let Some((placement, name)) = &self.0 {
            remove_module_cgroup(placement, name);
        }
    }
}

pub(crate) struct SpawnProgramBinding {
    pub(crate) path: PathBuf,
    // Keep the inode alive if the configured name is replaced while preparation
    // is suspended, so replacement cannot pass through inode reuse.
    pub(super) _file: std::fs::File,
    pub(crate) identity: subc_os::RetainedFileIdentity,
}

impl SpawnProgramBinding {
    pub(crate) fn capture(spec: &ModuleSpec) -> Result<Self, SuperviseError> {
        Self::capture_after_resolution(spec, |_| {})
    }

    pub(super) fn capture_after_resolution(
        spec: &ModuleSpec,
        before_open: impl FnOnce(&std::path::Path),
    ) -> Result<Self, SuperviseError> {
        require_program_search_path(spec)?;
        #[cfg(windows)]
        let windows_capture =
            windows_spawn_capture(spec).map_err(|source| SuperviseError::Spawn {
                program: spec.program.clone(),
                source,
                cgroup_path: None,
            })?;
        #[cfg(windows)]
        let path = windows_capture.path().to_path_buf();
        #[cfg(not(windows))]
        let path = if spec.program.is_absolute() || spec.program.components().count() > 1 {
            spec.program.clone()
        } else {
            let path = spec
                .env
                .iter()
                .rev()
                .find(|(key, _)| key == "PATH")
                .map(|(_, path)| std::ffi::OsString::from(path))
                .or_else(|| std::env::var_os("PATH"))
                .unwrap_or_else(|| {
                    if cfg!(target_os = "macos") {
                        // Match Darwin's spawn search when PATH is absent, not
                        // the current-directory search of an explicitly empty PATH.
                        "/usr/bin:/bin".into()
                    } else {
                        Default::default()
                    }
                });
            std::env::split_paths(&path)
                .map(|dir| dir.join(&spec.program))
                .find(|path| {
                    if !path.is_file() {
                        return false;
                    }
                    #[cfg(unix)]
                    {
                        rustix::fs::accessat(
                            rustix::fs::CWD,
                            path,
                            rustix::fs::Access::EXEC_OK,
                            rustix::fs::AtFlags::EACCESS,
                        )
                        .is_ok()
                    }
                    #[cfg(not(unix))]
                    {
                        true
                    }
                })
                .ok_or_else(|| SuperviseError::Spawn {
                    program: spec.program.clone(),
                    source: io::Error::new(io::ErrorKind::NotFound, "executable not found in PATH"),
                    cgroup_path: None,
                })?
        };
        let resolved = std::fs::canonicalize(&path).map_err(|source| SuperviseError::Spawn {
            program: spec.program.clone(),
            source,
            cgroup_path: None,
        })?;
        before_open(&resolved);
        // Resolving links does not open their target. The descriptor open must
        // remain nonblocking and no-follow if that resolved name is substituted.
        let file =
            subc_os::open_regular_nofollow(&resolved).map_err(|source| SuperviseError::Spawn {
                program: spec.program.clone(),
                source: io::Error::other(source),
                cgroup_path: None,
            })?;
        let identity =
            subc_os::retained_file_identity(&file).map_err(|source| SuperviseError::Spawn {
                program: spec.program.clone(),
                source,
                cgroup_path: None,
            })?;
        Ok(Self {
            path,
            _file: file,
            identity,
        })
    }
    pub(super) fn current(&self) -> bool {
        #[cfg(unix)]
        {
            subc_os::file_identity(&self.path) == Some(self.identity)
        }
        #[cfg(windows)]
        {
            std::fs::canonicalize(&self.path)
                .ok()
                .and_then(|path| subc_os::open_regular_nofollow(&path).ok())
                .and_then(|file| subc_os::retained_file_identity(&file).ok())
                == Some(self.identity)
        }
    }
}

pub(super) fn spawn_plan_stale(spec: &ModuleSpec) -> SuperviseError {
    SuperviseError::ConfigDeliveryDeferred {
        module_id: spec.module_id.clone(),
    }
}

fn require_program_search_path(spec: &ModuleSpec) -> Result<(), SuperviseError> {
    if !cfg!(target_os = "macos")
        && !spec.program.is_absolute()
        && spec.program.components().count() == 1
        && !spec.env.iter().any(|(key, _)| {
            if cfg!(windows) {
                key.eq_ignore_ascii_case("PATH")
            } else {
                key == "PATH"
            }
        })
        && std::env::var_os("PATH").is_none()
    {
        // The OS default search is not a current-directory search. Refusing an
        // unspecified search path prevents approving a decoy in the current directory.
        return Err(SuperviseError::Spawn {
            program: spec.program.clone(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "bare program requires PATH"),
            cgroup_path: None,
        });
    }
    Ok(())
}

pub(super) async fn spawn_child_async(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
) -> Result<SupervisedChild, SuperviseError> {
    #[cfg(all(test, unix))]
    if let Some(source) = containment_tests::join_failure(spec, &runtime.child_roster) {
        containment::terminate_blocking(&runtime.child_roster.containment);
        return Err(SuperviseError::Spawn {
            program: spec.program.clone(),
            source,
            cgroup_path: None,
        });
    }
    let result = spawn_child_in_slot_async(spec, runtime, SpawnRole::Plain, false).await;
    if result
        .as_ref()
        .is_err_and(|error| !is_launch_deferral(error))
    {
        if runtime.child_roster.admission_pending(&spec.module_id) {
            containment::terminate_deferred(&runtime.child_roster.containment);
        } else {
            containment::terminate_blocking(&runtime.child_roster.containment);
        }
    }
    result
}

pub(super) async fn spawn_child_in_slot_async(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    role: SpawnRole,
    alternate_slot: bool,
) -> Result<SupervisedChild, SuperviseError> {
    let gate = runtime
        .config_delivery
        .as_ref()
        .map(|delivery| delivery.0.module_gate(&spec.module_id))
        .transpose()
        .map_err(|error| SuperviseError::InvalidSpec {
            reason: error.to_string(),
        })?;
    if gate.as_ref().is_some_and(|gate| gate.try_lock().is_err()) {
        return Err(SuperviseError::ConfigDeliveryDeferred {
            module_id: spec.module_id.clone(),
        });
    }
    let prepared_spec = spec.clone();
    let prepared_runtime = runtime.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        let prepared = prepare_child_in_slot(
            &prepared_spec,
            prepared_runtime.connection_file_path.as_deref(),
            prepared_runtime.supervisor_handle.as_ref(),
            prepared_runtime.capture_logs_dir.as_deref(),
            &prepared_runtime.child_roster,
            prepared_runtime.spawn_delivery(),
            #[cfg(target_os = "linux")]
            prepared_runtime.cgroup_placement.as_ref(),
            role,
            alternate_slot,
            true,
        );
        prepared
    })
    .await
    .map_err(|_| SuperviseError::InvalidSpec {
        reason: "configuration launch preparation stopped".into(),
    })??;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let (Some(ticket), Some(launch)) =
        (&runtime.replacement_ticket, &prepared.registration_spawn)
    {
        ticket.bind(launch);
    }
    if let Some(registration) = &prepared.delivery_registration {
        if !registration.launch_current()? {
            return Err(spawn_plan_stale(spec));
        }
    }
    let _gate = match gate {
        Some(gate) => {
            Some(
                gate.try_lock_owned()
                    .map_err(|_| SuperviseError::ConfigDeliveryDeferred {
                        module_id: spec.module_id.clone(),
                    })?,
            )
        }
        None => None,
    };
    if let (Some(delivery), Some(admission)) = (runtime.spawn_delivery(), &prepared.spawn_admission)
    {
        delivery.check(spec, admission)?;
    }
    if prepared
        .spawn_admission
        .as_ref()
        .is_none_or(|admission| admission.launch.is_none())
        && prepared
            .program_binding
            .as_ref()
            .is_some_and(|binding| !binding.current())
    {
        return Err(spawn_plan_stale(spec));
    }
    // No await may separate this last identity check from ownership publication.
    let checked_base = prepared
        .spawn_admission
        .as_ref()
        .and_then(|admission| admission.approved_base())
        .cloned();
    let mut result = launch_child_in_slot(
        spec,
        &runtime.stderr_ring,
        &runtime.child_roster,
        prepared,
        runtime.register_timeout,
    );
    drop(_gate);
    if let (Some(delivery), Err(SuperviseError::ConfigHeld { code, detail, .. })) =
        (runtime.config_delivery.as_ref(), &result)
    {
        if let Some(base) = checked_base.filter(|_| code == "program_refused") {
            let module = spec.module_id.clone();
            let identity = subc_config::raw_digest(detail.as_bytes());
            let recorded = delivery
                .0
                .on_blocking(move |owner| owner.record_program_refusal(&module, &base, identity))
                .await;
            match recorded {
                Ok(true) => {}
                Ok(false) => result = Err(spawn_plan_stale(spec)),
                Err(error) => {
                    result = Err(config_delivery::gate_hold_error(
                        spec,
                        error,
                        Some(delivery),
                    ))
                }
            }
        }
    }
    result
}

fn validate_spawn_program(
    spec: &ModuleSpec,
    admission: Option<&crate::config_gate::runtime::SpawnAdmission>,
    delivery: Option<config_delivery::SpawnDelivery<'_>>,
) -> Result<(), SuperviseError> {
    if let Some(admission) = admission {
        if let Some(program) = &admission.launch {
            if let Err(error) = program.validate_for_spawn() {
                // A captured unsigned image is a definite refusal. Uncertain
                // reads are deferred by validate_for_spawn, not attributed to
                // the operator as a changed or unapprovable program.
                if let (Some(context), Some(base)) = (delivery, admission.approved_base()) {
                    if error.code == "program_refused"
                        && !context.record_refusal(spec, base, &error.detail)?
                    {
                        return Err(spawn_plan_stale(spec));
                    }
                }
                return Err(config_delivery::gate_hold_error(
                    spec,
                    error,
                    delivery.map(|context| context.runtime()).as_ref(),
                ));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_child_in_slot(
    spec: &ModuleSpec,
    connection_file_path: Option<&std::path::Path>,
    handle: Option<&SupervisorHandle>,
    capture_logs_dir: Option<&std::path::Path>,
    roster: &ChildRoster,
    config_delivery: Option<config_delivery::SpawnDelivery<'_>>,
    #[cfg(target_os = "linux")] cgroup_placement: Option<&subc_cgroup::Placement>,
    role: SpawnRole,
    alternate_slot: bool,
    asynchronous: bool,
) -> Result<PreparedChild, SuperviseError> {
    require_program_search_path(spec)?;
    let spawn_admission = config_delivery
        .map(|delivery| delivery.prepare(spec))
        .transpose()?;
    #[cfg(target_os = "macos")]
    let gated = spawn_admission
        .as_ref()
        .is_some_and(|admission| admission.approved_base().is_some());
    let mut launch_spec = spawn_admission
        .as_ref()
        .and_then(|admission| admission.launch.as_ref())
        .map(|launch| launch.spec.clone())
        .unwrap_or_else(|| spec.clone());
    if let Some(script) = &launch_spec.script {
        launch_spec.program =
            script::runner_path(script.runtime).map_err(|source| SuperviseError::Spawn {
                program: launch_spec.program.clone(),
                source,
                cgroup_path: None,
            })?;
    }
    let script_content = if let Some(entry) = &launch_spec.script {
        Some(
            spawn_admission
                .as_ref()
                .and_then(|admission| admission.launch.as_ref())
                .and_then(|launch| launch.script.clone())
                .map_or_else(|| script::ScriptBytes::read(&entry.path), Ok)
                .map_err(|error| config_delivery::gate_hold_error(&launch_spec, error, None))?,
        )
    } else {
        None
    };
    let spec = &launch_spec;
    if let Some(error) = recovery_block_error(spec, roster) {
        return Err(error);
    }
    if let Some((pid, reason)) = roster.previous_instance(&spec.module_id) {
        return Err(previous_instance_error(spec, pid, reason));
    }
    if roster.is_closed() {
        return Err(SuperviseError::Spawn {
            program: spec.program.clone(),
            source: io::Error::other("the daemon is shutting down; not starting a new process"),
            cgroup_path: None,
        });
    }
    let carried = containment::current(&roster.containment);
    let carrying = carries_containment(spec.carry_containment, carried.is_some());
    if role != SpawnRole::Plain && carrying {
        return Err(SuperviseError::InvalidSpec {
            reason: "a carried container cannot overlap a swap candidate".to_owned(),
        });
    }
    #[cfg(unix)]
    let launch_identity = if carrying && carried.is_none() {
        Some(
            containment::LaunchIdentity::capture().map_err(|source| SuperviseError::Spawn {
                program: spec.program.clone(),
                source,
                cgroup_path: None,
            })?,
        )
    } else {
        None
    };
    #[cfg(target_os = "linux")]
    let cgroup_placement = match &carried {
        Some(container) => container.cgroup().map(|(placement, _)| placement),
        None => cgroup_placement,
    };
    #[cfg(unix)]
    let group_target = if let Some(container) = carried
        .as_ref()
        .filter(|container| container.group().is_some())
    {
        containment::GroupTarget::Pinned(Arc::clone(container))
    } else {
        #[cfg(target_os = "linux")]
        let cgroup_container = carrying && cgroup_placement.is_some();
        #[cfg(not(target_os = "linux"))]
        let cgroup_container = false;
        if cgroup_container {
            containment::GroupTarget::NoGroup
        } else {
            containment::GroupTarget::OwnLeader
        }
    };
    #[cfg(target_os = "linux")]
    let cgroup_name = {
        // Slot names alone are not kill domains: a retired incumbent may still
        // be draining when a later enable/restart spawns into the same slot.
        // Decimal entropy keeps the suffix unambiguous; Placement performs
        // the module-id escaping and constructs the filesystem path.
        if let Some((_, name)) = carried.as_ref().and_then(|container| container.cgroup()) {
            name.to_owned()
        } else if cgroup_placement.is_none() {
            swap::cgroup_name(&spec.module_id, alternate_slot)
        } else {
            let nonce = generate_launch_nonce()?;
            let suffix = u128::from_str_radix(&nonce[..32], 16).expect("hex launch nonce");
            cgroup_label::generation_label(&spec.module_id, alternate_slot, suffix)
        }
    };
    #[cfg(not(target_os = "linux"))]
    let _ = alternate_slot;
    #[cfg(windows)]
    let image_capture = windows_spawn_capture(spec).map_err(|source| SuperviseError::Spawn {
        program: spec.program.clone(),
        source,
        cgroup_path: None,
    })?;
    #[cfg(windows)]
    let program_binding = if asynchronous {
        let mut resolved = spec.clone();
        resolved.program = image_capture.path().to_path_buf();
        Some(SpawnProgramBinding::capture(&resolved)?)
    } else {
        None
    };
    #[cfg(not(windows))]
    let program_binding = (asynchronous || cfg!(target_os = "macos"))
        .then(|| SpawnProgramBinding::capture(spec))
        .transpose()?;
    #[cfg(target_os = "macos")]
    validate_spawn_program(spec, spawn_admission.as_ref(), config_delivery)?;
    #[cfg(target_os = "macos")]
    let (mut command, privacy_exec, exec_ack) = privacy_command(
        spec,
        program_binding
            .as_ref()
            .expect("macOS captures every launch"),
        roster,
        // Ungated launches retain legacy unsuspended exec, including scripts. Suspended
        // verification admits approved programs only. prepare() above fails
        // closed if the gate cannot decide; an image sample is not authority.
        gated,
    )?;
    #[cfg(windows)]
    let mut command = Command::new(image_capture.path());
    #[cfg(not(any(target_os = "macos", windows)))]
    let mut command = Command::new(
        program_binding
            .as_ref()
            .map_or(spec.program.as_path(), |binding| binding.path.as_path()),
    );
    command.args(&spec.args);
    // AMBIENT `CK_LOG` MUST NOT LEAK INTO AN OTHERWISE UNCONFIGURED MODULE — but
    // that is the whole of the intent, so remove that one key rather than the
    // environment.
    //
    // This was `env_clear()` from 0.17.41 until 0.18.3, which achieved the goal
    // and took the POSIX environment with it. Modules spawned that way had no
    // HOME, XDG_RUNTIME_DIR, TMPDIR or USER, and the consequences ran past
    // logging:
    //
    //   * `connection_file::discover` reads XDG_RUNTIME_DIR and HOME, so with
    //     both unset it fell back to the temp dir alone and `ck` could not find
    //     a daemon running on the same machine from inside any module's process
    //     tree — reporting a path the file has never lived at, which reads as
    //     "the daemon did not write its file".
    //   * `default_data_home()` with HOME and XDG_DATA_HOME both unset returns
    //     the RELATIVE `.local/share`, so a module deriving its own store path
    //     resolved it against its own CWD. That is the store-fragmentation
    //     defect the daemon already refuses in config (`parse_doc` rejects a
    //     relative `storage.data_home`) arriving by derivation instead.
    //   * anything a module spawns inherited it: git without ~/.gitconfig,
    //     cargo without CARGO_HOME, ssh, python user dirs — all degrading
    //     quietly rather than erroring.
    //
    // Reported by iceteaSA as #104 after deploying 0.18.2, where `ck daemon`
    // offered one candidate under /tmp while the file sat in /run/user/1000.
    //
    // A configured module is unaffected either way: `module_spec()` puts the
    // resolved CK_LOG into `spec.env`, which is applied below and therefore
    // wins over anything ambient.
    apply_child_env(&mut command, spec);
    if spec.script.is_some() {
        script::remove_runtime_env(&mut command, spec);
    }
    let origin_witness = crate::program_origin::LaunchWitness::capture_command(spec, &command)
        .map_err(|source| SuperviseError::Spawn {
            program: spec.program.clone(),
            source,
            cgroup_path: None,
        })?;
    command.current_dir(&origin_witness.working_directory);
    apply_spawn_role(&mut command, role);
    let nonce_handoff = apply_wire_spawn_args_for_admission(
        &mut command,
        spec,
        connection_file_path,
        handle,
        role,
        spawn_admission.as_ref(),
    )?;
    #[cfg(unix)]
    let mut slots = subc_os::registration_channel::ChildDescriptorSlots::default();
    command.env_remove(subc_os::script_pipe::SCRIPT_FD_ENV);
    let script_delivery = script_content
        .map(|content| {
            script::Delivery::prepare(
                content,
                &mut command,
                #[cfg(unix)]
                &mut slots,
            )
        })
        .transpose()
        .map_err(|source| SuperviseError::Spawn {
            program: spec.program.clone(),
            source,
            cgroup_path: None,
        })?;
    #[cfg(unix)]
    let registration_channel = if spec.protocol == ModuleProtocol::Subc {
        let (parent, child, metadata) = subc_os::registration_channel::registration_socketpair()
            .map_err(|source| SuperviseError::Spawn {
                program: spec.program.clone(),
                source,
                cgroup_path: None,
            })?;
        slots
            .channel(child)
            .map_err(|source| SuperviseError::Spawn {
                program: spec.program.clone(),
                source,
                cgroup_path: None,
            })?;
        command
            .env(subc_os::registration_channel::REGISTRATION_FD_ENV, metadata)
            .env(
                subc_os::registration_channel::DAEMON_PID_ENV,
                std::process::id().to_string(),
            );
        Some(parent)
    } else {
        None
    };
    #[cfg(windows)]
    let registration_channel = prepare_windows_registration_pipe(&mut command, spec)?;
    let binding_publication = if spawn_admission
        .as_ref()
        .is_some_and(|admission| admission.launch.is_some())
    {
        handle.and_then(|handle| handle.begin_program_binding(&spec.module_id, role))
    } else {
        None
    };
    let registration_spawn = (spec.protocol == ModuleProtocol::Subc)
        .then(|| {
            handle.map(|handle| {
                let spawn = RegistrationSpawn::new_with_binding_requirement(
                    handle,
                    &spec.module_id,
                    role,
                    binding_publication.clone(),
                    spawn_admission
                        .as_ref()
                        .is_some_and(|admission| admission.launch.is_some()),
                );
                if let Some(admission) = &spawn_admission {
                    admission.initialize_origin(&spawn.authority, &origin_witness);
                }
                handle.record_nonce_launch(&spawn);
                spawn
            })
        })
        .flatten();

    #[cfg(target_os = "linux")]
    let cgroup_path = cgroup_placement
        .map(|placement| {
            if let Some(container) = &carried {
                let path = container
                    .cgroup_path()
                    .ok_or_else(|| io::Error::other("carried container has no cgroup path"))?;
                placement.existing_module_path(path)?.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "carried cgroup disappeared before replacement",
                    )
                })
            } else {
                if carrying {
                    containment::prepare_cgroup_path(placement, &cgroup_name)
                } else {
                    placement.module_path(&cgroup_name)
                }
            }
        })
        .transpose()
        .map_err(|source| SuperviseError::Cgroup {
            module_id: spec.module_id.clone(),
            source,
        })?;
    #[cfg(not(target_os = "linux"))]
    let cgroup_path: Option<PathBuf> = None;
    // Own a newly created directory before any fallible admission or command
    // preparation. Existing carried containment must never be removed here.
    #[cfg(target_os = "linux")]
    let cgroup_cleanup = PreparedCgroup(
        cgroup_placement
            .filter(|_| carried.is_none())
            .map(|placement| (placement.clone(), cgroup_name.clone())),
    );
    #[cfg(unix)]
    let mut admission_gate = if carrying {
        Some(
            admission::Gate::install(&mut command, spec, roster).map_err(|source| {
                SuperviseError::Spawn {
                    program: spec.program.clone(),
                    source,
                    cgroup_path: cgroup_path.clone(),
                }
            })?,
        )
    } else {
        None
    };
    #[cfg(target_os = "linux")]
    if let Some(path) = &cgroup_path {
        apply_cgroup_placement(&mut command, spec, path)?;
    }

    let output_sink = capture_sink(spec, capture_logs_dir);

    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(!carrying);
    // EACH MODULE LEADS ITS OWN PROCESS GROUP (the child calls setpgid(0, 0)
    // before exec). In the daemon's group, a service manager that kills the
    // job's process group when the daemon exits (launchd's default) killed
    // every module at the same moment its control connection closed, so no
    // module ever ran its EOF teardown on a daemon stop. Outside that group a
    // module is reached only by the daemon: the EOF it sees when its
    // connection closes, and the bounded stop in `child_roster` for anything
    // still running after that. On Linux this composes with the cgroup
    // placement above: that is a pre_exec write to cgroup.procs, std performs
    // setpgid in the child before running pre_exec callbacks, and the two
    // change independent process attributes.
    //
    // stdin is /dev/null because a process outside the terminal's foreground
    // group is stopped (SIGTTIN) if it reads the terminal, which a daemon run
    // by hand would otherwise hand down. Under a service manager stdin is
    // already /dev/null.
    #[cfg(unix)]
    command.process_group(
        carried
            .as_ref()
            .map(|container| container.verified_join_group())
            .transpose()
            .map_err(|source| SuperviseError::Spawn {
                program: spec.program.clone(),
                source,
                cgroup_path: cgroup_path.clone(),
            })?
            .flatten()
            .unwrap_or(0) as i32,
    );
    command.stdin(Stdio::null());
    #[cfg(unix)]
    if let Some(gate) = &mut admission_gate {
        gate.install_placement(&mut command);
    }
    // Fill all reserved slots in one allocation-free step, after placement has
    // stopped using descriptors that the fixed slots may replace.
    #[cfg(unix)]
    {
        if let Some(handoff) = nonce_handoff {
            slots.nonce(handoff);
        }
        #[cfg(target_os = "macos")]
        slots
            .acknowledgement(exec_ack.into_descriptor())
            .map_err(|source| SuperviseError::Spawn {
                program: spec.program.clone(),
                source,
                cgroup_path: cgroup_path.clone(),
            })?;
        slots.install(command.as_std_mut());
    }
    #[cfg(not(any(unix, windows)))]
    let _ = nonce_handoff;

    // Containment, step 1 of 3 (issue #109): create the child suspended so it
    // cannot run a single instruction -- and therefore cannot spawn a
    // grandchild -- before it is in the job. See `contain_spawned_child` for the
    // other two steps and why the window matters.
    #[cfg(windows)]
    {
        subc_jobobject::ensure_root_job().map_err(|source| SuperviseError::Spawn {
            program: spec.program.clone(),
            source,
            cgroup_path: None,
        })?;
        subc_jobobject::suspend_on_create_async(&mut command);
    }
    // Timestamp before the spawn call: on Unix the child can serve before that
    // call even returns. This conservative bound never labels an older child
    // as a replacement for a request received during process creation.
    let spawned_at = Instant::now();
    let delivery_context = config_delivery;
    let config_delivery = config_delivery
        .map(|delivery| {
            delivery.spawned(
                spec,
                spawn_admission
                    .as_ref()
                    .expect("delivery prepares spawn admission"),
                spawned_at,
                role,
                registration_spawn
                    .as_ref()
                    .map(|spawn| spawn.authority.clone()),
            )
        })
        .transpose()?
        .flatten();
    let mut delivery_registration =
        delivery_context.map(|context| context.registration_guard(config_delivery.as_ref(), role));
    if let Some(registration) = &mut delivery_registration {
        registration.defer_cleanup();
    }
    if asynchronous {
        #[cfg(test)]
        if let Some(context) = delivery_context {
            context.pause_preflight_for_test();
        }
    }
    validate_spawn_program(spec, spawn_admission.as_ref(), delivery_context)?;
    Ok(PreparedChild {
        script_delivery,
        registration_spawn,
        registration_handle: handle.cloned(),
        #[cfg(any(unix, windows))]
        registration_channel,
        config_context: delivery_context.map(|delivery| delivery.runtime()),
        binding_publication,
        launch_spec,
        spawn_admission,
        command,
        carrying,
        carried: carried.clone(),
        #[cfg(unix)]
        launch_identity,
        #[cfg(unix)]
        group_target,
        #[cfg(target_os = "linux")]
        cgroup_placement: cgroup_placement.cloned(),
        #[cfg(target_os = "linux")]
        cgroup_cleanup,
        #[cfg(target_os = "linux")]
        cgroup_name,
        cgroup_path,
        #[cfg(unix)]
        admission_gate,
        output_sink,
        #[cfg(target_os = "macos")]
        privacy_exec,
        #[cfg(windows)]
        image_capture,
        #[cfg(windows)]
        nonce_handoff,
        spawned_at,
        config_delivery,
        delivery_registration,
        program_binding,
    })
}

pub(super) fn launch_child_in_slot(
    _spec: &ModuleSpec,
    ring: &Arc<Mutex<StderrRing>>,
    roster: &ChildRoster,
    prepared: PreparedChild,
    register_timeout: Duration,
) -> Result<SupervisedChild, SuperviseError> {
    let launch_spec = prepared.launch_spec.clone();
    let registration_declaration = prepared
        .spawn_admission
        .as_ref()
        .and_then(|admission| admission.registration_declaration(&launch_spec.module_id));
    let admission_base = prepared
        .spawn_admission
        .as_ref()
        .and_then(|admission| admission.approved_base())
        .cloned();
    let spec = &launch_spec;
    let expected_image = prepared
        .spawn_admission
        .as_ref()
        .and_then(|admission| admission.launch.as_ref())
        .map(|launch| launch.image);
    let program_launch = prepared
        .spawn_admission
        .and_then(|admission| admission.launch);
    let config_owner = prepared
        .config_context
        .as_ref()
        .map(|context| context.0.clone());
    let binding_publication = prepared.binding_publication;
    #[cfg(test)]
    if let Some(context) = &prepared.config_context {
        context.0.before_spawn_exec_for_test();
    }
    #[cfg(feature = "test-support")]
    if let Some(context) = &prepared.config_context {
        context
            .0
            .pause_spawn_before_exec()
            .map_err(|error| config_delivery::gate_hold_error(spec, error, Some(context)))?;
    }
    let mut command = prepared.command;
    let carrying = prepared.carrying;
    let carried = prepared.carried;
    #[cfg(unix)]
    let launch_identity = prepared.launch_identity;
    #[cfg(unix)]
    let group_target = prepared.group_target;
    #[cfg(target_os = "linux")]
    let cgroup_placement = prepared.cgroup_placement.as_ref();
    #[cfg(target_os = "linux")]
    let cgroup_name = prepared.cgroup_name;
    #[cfg(target_os = "linux")]
    let mut cgroup_cleanup = prepared.cgroup_cleanup;
    #[cfg(target_os = "linux")]
    let _ = cgroup_cleanup.0.take();
    let cgroup_path = prepared.cgroup_path;
    #[cfg(unix)]
    let admission_gate = prepared.admission_gate;
    let output_sink = prepared.output_sink;
    #[cfg(target_os = "macos")]
    let mut privacy_exec = prepared.privacy_exec;
    #[cfg(target_os = "macos")]
    if let Some(pending) = &mut privacy_exec {
        pending.admission_binding = binding_publication.clone();
        if expected_image.is_some() {
            pending.expected = expected_image;
        }
    }
    #[cfg(any(unix, windows))]
    let registration_channel = prepared.registration_channel;
    let registration_spawn = prepared.registration_spawn;
    let registration_handle = prepared.registration_handle;
    #[cfg(windows)]
    let image_capture = prepared.image_capture;
    #[cfg(windows)]
    let nonce_handoff = prepared.nonce_handoff;
    let spawned_at = prepared.spawned_at;
    let config_delivery = prepared.config_delivery;
    let delivery_registration = prepared.delivery_registration;
    let _program_binding = prepared.program_binding;
    #[cfg(all(test, unix))]
    containment_tests::record_spawn_attempt(spec, &mut command);
    #[cfg(unix)]
    let spawned = match admission_gate {
        Some(gate) => gate
            .spawn(
                command,
                spec,
                roster,
                launch_identity,
                #[cfg(target_os = "linux")]
                cgroup_placement
                    .zip(cgroup_path.as_ref())
                    .map(|(placement, path)| {
                        (placement.clone(), cgroup_name.clone(), path.clone())
                    }),
            )
            .map(|(child, admitted)| (child, Some(admitted))),
        None => {
            let spawned =
                subc_os::child_spawn::tokio_spawn(&mut command).map(|child| (child, None));
            // The callback owns all child-only sources. Closing them here is
            // necessary for channel EOF and macOS exec acknowledgement EOF.
            drop(command);
            spawned
        }
    };
    #[cfg(not(unix))]
    let spawned = subc_os::child_spawn::tokio_spawn(&mut command);
    let spawned = match spawned {
        Ok(child) => child,
        Err(source) => {
            if carried
                .as_ref()
                .and_then(|container| container.group())
                .is_some()
            {
                containment::terminate_blocking(&roster.containment);
            }
            #[cfg(target_os = "linux")]
            if let Some(placement) = cgroup_placement.filter(|_| carried.is_none()) {
                remove_module_cgroup(placement, &cgroup_name);
            }
            return Err(SuperviseError::Spawn {
                program: spec.program.clone(),
                source,
                cgroup_path,
            });
        }
    };
    #[cfg(unix)]
    let (mut child, mut admitted) = spawned;
    #[cfg(unix)]
    let group_target = admitted
        .as_ref()
        .map(|attempt| attempt.group_target())
        .unwrap_or(group_target);
    #[cfg(not(unix))]
    let mut child = spawned;
    if let Some(delivery) = prepared.script_delivery {
        delivery.start(child.id().expect("spawned child has a pid"));
    }
    #[cfg(test)]
    spawn_record_tests::after_create(&child, spec);

    // Bind while the child is still suspended, before job assignment resumes it.
    // Keeping the executable handle open across CreateProcessW denies rename
    // and deletion, so the file cannot be exchanged during spawn. Bind that
    // file ID to the process handle returned by spawn, never a reopened PID.
    #[cfg(windows)]
    let windows_image = match image_capture.bind_tokio(&child) {
        Ok(image) => Arc::new(image),
        Err(source) => {
            kill_and_reap_rejected_child(&mut child);
            return Err(SuperviseError::Spawn {
                program: spec.program.clone(),
                source,
                cgroup_path: None,
            });
        }
    };
    // Assign the suspended child to its module job; resume only after its
    // process identity has been durably recorded for orphan recovery.
    #[cfg(windows)]
    let job = contain_spawned_child(&mut child, spec, carried.as_ref())?;
    let spawned_at_ms = unix_ms_now();
    #[cfg(not(windows))]
    let spawned_from = spec.program.clone();
    #[cfg(windows)]
    let spawned_from = windows_image.path().to_path_buf();
    #[cfg(not(windows))]
    let spawned_file_identity = spawned_file_identity(&spawned_from);
    #[cfg(windows)]
    let spawned_file_identity = Some(SpawnedFileIdentity {
        device: 0,
        inode: 0,
        windows_image: Some(Arc::clone(&windows_image)),
    });
    let pid = child.id().ok_or_else(|| SuperviseError::Spawn {
        program: spec.program.clone(),
        source: io::Error::other("spawned child exposed no live pid"),
        cgroup_path: cgroup_path.clone(),
    })?;
    #[cfg(unix)]
    let plain_group = (!carrying
        && cgroup_path.is_none()
        && matches!(group_target, containment::GroupTarget::OwnLeader))
    .then(|| plain_group::PlainGroup::from_spawn(pid).map(Arc::new))
    .flatten();
    #[cfg(windows)]
    let nonce_delivery = start_windows_nonce_pipe(nonce_handoff, pid, spec);
    #[cfg(not(windows))]
    let process_start_time = admitted
        .as_ref()
        .map(|admitted| admitted.birth)
        .or_else(|| crate::provenance::process_start_time(pid));
    #[cfg(windows)]
    let process_start_time = Some(windows_image.start_time());
    let process_identity = process_start_time.map(|start_time| ProcessIdentity { pid, start_time });
    #[cfg(all(test, target_os = "macos"))]
    privacy_exec_boundary_tests::before_image_sample(spec, pid);
    #[cfg(all(test, target_os = "linux"))]
    assert!(roster.admission_event_for_test(&spec.module_id, "before_image_binding", Some(pid)));
    // Record the committed kernel image, not the configured pathname: PATH
    // lookup and shebang interpretation can select a different executable.
    // Linux's posix_spawn can return before that image is installed, so the
    // observer also waits for the kernel's first-exec flag to clear.
    #[cfg(not(windows))]
    let recorded_image = observe_spawned_image(pid);
    #[cfg(all(test, target_os = "linux"))]
    let recorded_image = if roster.admission_event_for_test(
        &spec.module_id,
        "after_spawn_image_sample",
        Some(pid),
    ) {
        recorded_image
    } else {
        None
    };
    #[cfg(target_os = "linux")]
    // An exited child cannot receive HELLO service and has no readable /proc/exe.
    // Keep its real exit for the supervisor instead of turning a crash into a hold.
    let exited_before_binding = expected_image.is_some()
        && recorded_image.and_then(|image| image.executable) != expected_image
        && subc_os::child_exit::observe(pid).ok().flatten().is_some();
    #[cfg(target_os = "linux")]
    let unavailable_initial_image = expected_image.is_some()
        && recorded_image.and_then(|image| image.executable).is_none()
        && !exited_before_binding;
    #[cfg(not(target_os = "linux"))]
    let unavailable_initial_image = false;
    #[cfg(target_os = "linux")]
    if expected_image
        .is_some_and(|expected| recorded_image.and_then(|image| image.executable) != Some(expected))
        && !exited_before_binding
        && !unavailable_initial_image
    {
        kill_module_cgroup(cgroup_placement, &cgroup_name);
        kill_and_reap_rejected_child(&mut child, group_target.clone());
        return Err(SuperviseError::ConfigHeld {
            module_id: spec.module_id.clone(),
            code: "program_refused".into(),
            detail: format!(
                "spawned executable {:?} does not match hashed program {:?}",
                recorded_image.and_then(|image| image.executable),
                expected_image
            ),
        });
    }
    #[cfg(not(target_os = "linux"))]
    let _ = expected_image;
    #[cfg(target_os = "macos")]
    let binding_ready = privacy_exec.is_none();
    #[cfg(target_os = "linux")]
    let binding_ready = !exited_before_binding;
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let binding_ready = cfg!(windows);
    // spawn() confirms only the first exec, into the trampoline. Never persist
    // the trampoline image; the asynchronous acknowledgement publishes the
    // module image once the trampoline has replaced itself with the module.
    #[cfg(target_os = "macos")]
    let recorded_image = if privacy_exec.is_some() {
        None
    } else {
        recorded_image
    };
    #[cfg(target_os = "linux")]
    let recorded_cgroup_name = cgroup_path.as_ref().map(|_| cgroup_name.clone());
    #[cfg(not(target_os = "linux"))]
    let recorded_cgroup_name = None;
    #[cfg(unix)]
    let preadmitted = admitted
        .as_mut()
        .and_then(|admitted| admitted.leader.take());
    #[cfg(not(unix))]
    let preadmitted: Option<crate::child_roster::RosterGuard> = None;
    let roster_guard = if let Some(guard) = preadmitted {
        Ok(guard)
    } else {
        roster.admit(
            spec.module_id.clone(),
            pid,
            spec.protocol,
            process_start_time,
            crate::child_roster::RecordedIdentity {
                #[cfg(target_os = "linux")]
                container: cgroup_path
                    .clone()
                    .filter(|_| !carrying)
                    .map(ordinary_cgroup::recorded_identity),
                #[cfg(not(target_os = "linux"))]
                container: None,
                #[cfg(unix)]
                carried: None,
                #[cfg(unix)]
                plain_group: plain_group.as_ref().map(Arc::downgrade),
                #[cfg(not(windows))]
                start_time: recorded_image.map(|image| image.start_time),
                #[cfg(windows)]
                start_time: process_start_time,
                #[cfg(not(windows))]
                executable: recorded_image
                    .and_then(|image| image.executable)
                    .map(crate::live_children::ExecutableIdentity::from),
                #[cfg(windows)]
                executable: Some(crate::live_children::ExecutableIdentity::from(
                    windows_image.identity(),
                )),
                cgroup_name: recorded_cgroup_name,
                #[cfg(target_os = "linux")]
                cgroup_placement: cgroup_placement.cloned(),
            },
        )
    };
    let roster_guard = match roster_guard {
        Ok(guard) => guard,
        Err(failure) => {
            return failure.with_cleanup(|source| {
                #[cfg(windows)]
                if let Some(job) = &job {
                    let _ = job.lock().unwrap_or_else(|p| p.into_inner()).terminate();
                }
                #[cfg(target_os = "linux")]
                kill_module_cgroup(cgroup_placement, &cgroup_name);
                #[cfg(unix)]
                let rejected_group = group_target.clone();
                kill_and_reap_rejected_child(
                    &mut child,
                    #[cfg(unix)]
                    rejected_group,
                );
                #[cfg(target_os = "linux")]
                if let Some(placement) = cgroup_placement.filter(|_| carried.is_none()) {
                    remove_module_cgroup(placement, &cgroup_name);
                }
                Err(SuperviseError::Spawn {
                    program: spec.program.clone(),
                    source: io::Error::other(format!(
                        "could not durably record module child: {source}"
                    )),
                    cgroup_path,
                })
            });
        }
    };
    #[cfg(unix)]
    if admitted.is_some() {
        if let Some(image) = recorded_image {
            if let Err(source) = roster_guard.confirm_executable(image) {
                kill_and_reap_rejected_child(&mut child, group_target.clone());
                return Err(SuperviseError::Spawn {
                    program: spec.program.clone(),
                    source,
                    cgroup_path,
                });
            }
        }
    }
    // The pre-spawn check can pass just before shutdown begins. Ownership is
    // published at the start of admission, even while a record write is queued.
    // If shutdown already saw an empty roster before that publication, this
    // check sees the roster closed and ends the child before starting a module.
    if roster.is_closed() {
        if carrying {
            #[cfg(unix)]
            let rejected_group = admitted
                .as_ref()
                .map(|admitted| admitted.rejection_target())
                .unwrap_or_else(|| group_target.clone());
            #[cfg(windows)]
            if let Some(job) = &job {
                let _ = job.lock().unwrap_or_else(|p| p.into_inner()).terminate();
            }
            kill_and_reap_rejected_child(
                &mut child,
                #[cfg(unix)]
                rejected_group,
            );
            #[cfg(all(test, unix))]
            let _ =
                roster.admission_event_for_test(&spec.module_id, "after_rejected_child", Some(pid));
            drop(roster_guard);
            return Err(SuperviseError::Spawn {
                program: spec.program.clone(),
                source: io::Error::other(
                    "the daemon began shutting down during carried admission; ended the container",
                ),
                cgroup_path,
            });
        }
        // This child was never admitted, so there is no module protocol shutdown to wait for.
        #[cfg(target_os = "linux")]
        kill_module_cgroup(cgroup_placement, &cgroup_name);
        if let Err(error) = child.start_kill() {
            debug!(module_id = %spec.module_id, pid, %error, "kill of a process spawned during daemon shutdown failed; it may already have exited");
        }
        #[cfg(target_os = "linux")]
        if let Some(placement) = cgroup_placement.filter(|_| carried.is_none()) {
            // This spawn was never admitted, so shutdown has no roster entry
            // to await. Do not detach its cleanup: the runtime could exit
            // before that task reaps the rejected child and removes its group.
            while matches!(child.try_wait(), Ok(None)) {
                std::thread::yield_now();
            }
            if matches!(
                subc_cgroup::kill_module(Some(placement), &cgroup_name),
                subc_cgroup::KillOutcome::Killed
            ) {
                if let Ok(path) = placement.module_path(&cgroup_name) {
                    while std::fs::read_to_string(path.join("cgroup.events"))
                        .ok()
                        .is_some_and(|events| events.lines().any(|line| line == "populated 1"))
                    {
                        std::thread::yield_now();
                    }
                }
            }
            remove_module_cgroup(placement, &cgroup_name);
        }
        drop(roster_guard);
        return Err(SuperviseError::Spawn {
            program: spec.program.clone(),
            source: io::Error::other(
                "the daemon began shutting down while this process was starting; ended it",
            ),
            cgroup_path,
        });
    }

    let pumps = crate::child_output::start(
        &mut child,
        output_sink,
        ring,
        &spec.module_id,
        #[cfg(unix)]
        plain_group.is_some(),
    );

    let exec = Arc::new(crate::activity::ExecOutcome::new());
    #[cfg(target_os = "macos")]
    let pending_exec = privacy_exec.is_some();
    #[cfg(not(target_os = "macos"))]
    let pending_exec = false;
    if !pending_exec {
        exec.finish(crate::activity::ExecState::CommittedExec);
    }
    #[cfg(windows)]
    let subtree_target = job
        .as_ref()
        .map(|job| crate::subtree_resources::Target::Job(Arc::downgrade(job)))
        .unwrap_or(crate::subtree_resources::Target::Unavailable("no_job"));
    #[cfg(target_os = "linux")]
    let subtree_target = cgroup_path
        .map(crate::subtree_resources::Target::Cgroup)
        .unwrap_or(crate::subtree_resources::Target::Unavailable("no_cgroup"));
    #[cfg(not(any(target_os = "linux", windows)))]
    let subtree_target = crate::subtree_resources::Target::Unavailable("unsupported_platform");
    let mut supervised = SupervisedChild {
        registration_spawn,
        #[cfg(any(unix, windows))]
        registration_channel,
        live_program_binding: None,
        registered_config_connection: None,
        admission_base,
        registration_declaration,
        _program_binding: binding_publication,
        subtree_target,
        child: containment::ProcessChild::plain(child, pid),
        exec: crate::activity::ExecOwner(exec),
        #[cfg(windows)]
        nonce_delivery,
        protocol: spec.protocol,
        #[cfg(target_os = "linux")]
        module_id: cgroup_name,
        #[cfg(target_os = "linux")]
        cgroup_placement: cgroup_placement.cloned(),
        #[cfg(target_os = "linux")]
        unresolved_cgroup: None,
        #[cfg(windows)]
        job,
        stdout_pump: pumps.stdout,
        stderr_pump: pumps.stderr,
        #[cfg(unix)]
        capture_stop: pumps.stop,
        #[cfg(unix)]
        late_output_lost: false,
        stderr_ring: Arc::clone(ring),
        spawned_at,
        config_delivery,
        registration_deadline: None,
        registration_watch: None,
        spawned_at_ms,
        spawned_from,
        launch_spec_digest: LaunchSpecDigest::from_spec(spec),
        spawned_file_identity,
        process_start_time,
        process_identity,
        pid,
        roster_guard: Some(roster_guard),
        #[cfg(target_os = "macos")]
        privacy_exec,
        #[cfg(target_os = "macos")]
        report_ready: Arc::new(OnceLock::new()),
        spawn_failure: None,
        admission_kill_sent: false,
    };
    #[cfg(unix)]
    {
        supervised.child.plain_group = plain_group;
    }
    if let Err(error) = containment::adopt(&mut supervised, spec, roster) {
        supervised.child.reject(
            #[cfg(unix)]
            group_target,
        );
        #[cfg(all(test, unix))]
        let _ = roster.admission_event_for_test(&spec.module_id, "after_rejected_child", Some(pid));
        return Err(error);
    }
    if let (Some(launch), Some(owner), Some(base)) = (
        program_launch,
        config_owner,
        supervised.admission_base.clone(),
    ) {
        supervised.live_program_binding =
            content_binding::LiveProgramBinding::new(launch, &supervised, owner, base);
        if let (Some(publication), Some(live)) = (
            &supervised._program_binding,
            &supervised.live_program_binding,
        ) {
            publication.attach(live);
        }
    }
    #[cfg(unix)]
    if let Some(admitted) = admitted.take() {
        admitted.disarm();
    }
    #[cfg(windows)]
    if let Err(error) = resume_suspended_child(pid, spec) {
        containment::terminate_blocking(&roster.containment);
        let _ = supervised.child.start_kill();
        return Err(error);
    }
    if binding_ready {
        // An unreadable initial image is not a definite mismatch. Keep the
        // owned child and delivery registration until its reaped status decides
        // whether it crashed on its own or was stopped by admission's kill.
        let checked = if unavailable_initial_image {
            Err(crate::config_gate::GateError::new(
                "config_delivery_deferred",
                "initial process image is unavailable",
            ))
        } else {
            match (
                &supervised._program_binding,
                &supervised.live_program_binding,
            ) {
                (Some(binding), _) => binding.verify(),
                (None, Some(live)) => live.check(),
                _ if supervised.admission_base.is_some() => {
                    Err(crate::config_gate::GateError::new(
                        "config_delivery_deferred",
                        "live program witness is unavailable",
                    ))
                }
                _ => Ok(()),
            }
        };
        #[cfg(all(test, target_os = "linux"))]
        let checked = if roster.admission_event_for_test(
            &spec.module_id,
            "after_content_witness_check",
            Some(pid),
        ) {
            checked
        } else {
            Err(crate::config_gate::GateError::new(
                "config_delivery_deferred",
                "current process image is unavailable",
            ))
        };
        if let Err(error) = checked {
            if let Some(binding) = &mut supervised._program_binding {
                binding.reject();
            }
            // Reap through the normal child-reaping path, retaining the real exit record and
            // its roster ownership. A refusal must also end any helpers.
            if error.code == "program_refused" || supervised.try_wait().ok().flatten().is_none() {
                supervised.spawn_failure = Some(format!("{}: {}", error.code, error.detail));
                #[cfg(all(test, target_os = "linux"))]
                assert!(roster.admission_event_for_test(
                    &spec.module_id,
                    if unavailable_initial_image {
                        "before_initial_image_kill"
                    } else {
                        "before_content_witness_kill"
                    },
                    Some(pid),
                ));
                let _ = supervised.start_admission_kill();
                #[cfg(all(test, target_os = "linux"))]
                if supervised.admission_kill_sent {
                    assert!(roster.admission_event_for_test(
                        &spec.module_id,
                        "after_admission_kill_sent",
                        Some(pid),
                    ));
                }
            }
        }
    }
    if let Some(registration) = delivery_registration {
        registration.commit();
    }
    if let Some(spawn) = &supervised.registration_spawn {
        if let Some(record) = supervised
            .roster_guard
            .as_ref()
            .and_then(crate::child_roster::RosterGuard::resume_token_record)
        {
            let _ = spawn.resume_token_record.set(record);
        }
        if let Some(identity) = supervised.process_identity {
            let _ = spawn.identity.set(identity);
        }
        if supervised.spawn_failure.is_some() {
            spawn.refuse();
        } else if binding_ready {
            spawn.ready();
        }
        #[cfg(any(unix, windows))]
        if let (Some(handle), Some(channel)) =
            (registration_handle, supervised.registration_channel.take())
        {
            handle.serve_registration(channel, spawn.clone(), pid, spawned_at + register_timeout);
        }
    }
    Ok(supervised)
}

#[cfg(target_os = "linux")]
pub(crate) fn kill_module_cgroup(placement: Option<&subc_cgroup::Placement>, module_id: &str) {
    use subc_cgroup::KillOutcome;
    match subc_cgroup::kill_module(placement, module_id) {
        KillOutcome::Killed => {}
        KillOutcome::NotPlaced | KillOutcome::Unsupported => {
            debug!(
                module_id,
                "cgroup tree kill unavailable; using direct-child kill"
            );
        }
        KillOutcome::IoError { path, error } => {
            warn!(module_id, path = %path.display(), %error, "cgroup tree kill failed; using direct-child kill");
        }
    }
}

/// Assign a freshly spawned Windows child without ever resuming on failure.
///
/// Steps 2 and 3 of the suspended-create contract: the job is created and the
/// child assigned **while it is still suspended** (step 1 is
/// `suspend_on_create_async` at the spawn site). The caller resumes only after
/// the child's roster admission is durable. Failed containment kills and reaps
/// the suspended child instead of running an unprotected module.
#[cfg(windows)]
fn contain_spawned_child(
    child: &mut Child,
    spec: &ModuleSpec,
    carried: Option<&Arc<containment::CarriedContainment>>,
) -> Result<Option<Arc<Mutex<subc_jobobject::JobObject>>>, SuperviseError> {
    let assignment = || {
        let job = match carried {
            Some(container) => container.job(),
            None => Arc::new(Mutex::new(subc_jobobject::JobObject::new()?)),
        };
        #[cfg(test)]
        spawn_record_tests::assignment_failpoint(spec)?;
        job.lock()
            .unwrap_or_else(|p| p.into_inner())
            .assign(child)?;
        Ok::<_, io::Error>(job)
    };
    match assignment() {
        Ok(job) => Ok(Some(job)),
        Err(source) => {
            kill_and_reap_rejected_child(child);
            Err(SuperviseError::Spawn {
                program: spec.program.clone(),
                source: io::Error::other(format!(
                    "Windows module job containment failed: {source}"
                )),
                cgroup_path: None,
            })
        }
    }
}

/// Rejection is synchronous: do not return a spawn error while the child can
/// still run, or rely on kill-on-drop to reap it at some later runtime turn.
pub(super) fn kill_and_reap_rejected_child(
    child: &mut Child,
    #[cfg(unix)] group: containment::GroupTarget,
) {
    #[cfg(unix)]
    group.signal_rejected(child);
    let _ = child.start_kill();
    while matches!(child.try_wait(), Ok(None)) {
        std::thread::yield_now();
    }
}

/// Resume a suspended child, killing it if it cannot be started.
///
/// A suspended process holds a pid and does nothing, so there is no useful
/// state to return: the caller gets an error and the spawn fails.
#[cfg(windows)]
fn resume_suspended_child(pid: u32, spec: &ModuleSpec) -> Result<(), SuperviseError> {
    if let Err(source) = subc_jobobject::resume_main_thread(pid) {
        // Kill it here rather than leaving a suspended process for the caller
        // to notice; `kill_on_drop` would eventually do this, but the module
        // would have been reported as running in between.
        let _ = subc_os::child_spawn::status(
            std::process::Command::new("taskkill.exe")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        );
        return Err(SuperviseError::Spawn {
            program: spec.program.clone(),
            source,
            cgroup_path: None,
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn remove_module_cgroup(placement: &subc_cgroup::Placement, module_id: &str) {
    match placement.remove_module(module_id) {
        Ok(()) => debug!(module_id, "removed module cgroup after process exit"),
        Err(error) => warn!(
            module_id,
            error = %error,
            "could not remove module cgroup after process exit; continuing teardown"
        ),
    }
}

#[cfg(target_os = "linux")]
pub(super) fn apply_cgroup_placement(
    command: &mut Command,
    spec: &ModuleSpec,
    path: &std::path::Path,
) -> Result<(), SuperviseError> {
    subc_cgroup::apply(command, path).map_err(|source| SuperviseError::Cgroup {
        module_id: spec.module_id.clone(),
        source,
    })
}

fn capture_retention(spec: &ModuleSpec) -> Retention {
    let defaults = Retention::default();
    let value = |name: &str| {
        spec.env
            .iter()
            .rev()
            .find_map(|(key, value)| (key == name).then_some(value.as_str()))
    };
    Retention {
        max_file_mb: value(CAPTURE_MAX_FILE_MB_ENV)
            .and_then(|value| value.parse().ok())
            .unwrap_or(defaults.max_file_mb),
        keep: value(CAPTURE_KEEP_ENV)
            .and_then(|value| value.parse().ok())
            .unwrap_or(defaults.keep),
        max_age_days: value(CAPTURE_MAX_AGE_DAYS_ENV)
            .and_then(|value| value.parse().ok())
            .unwrap_or(defaults.max_age_days),
    }
}

pub(super) fn capture_sink(
    spec: &ModuleSpec,
    logs_dir: Option<&std::path::Path>,
) -> ChildOutputSink {
    let Some(logs_dir) = logs_dir else {
        return ChildOutputSink::Stderr;
    };
    let path = logs_dir.join(format!("{}.stderr.log", spec.module_id));
    match ChildOutputSink::open(&path, capture_retention(spec)) {
        Ok(sink) => sink,
        Err(error) => {
            warn!(module_id = %spec.module_id, path = %path.display(), %error,
                "could not open child output capture file; forwarding to stderr");
            ChildOutputSink::Stderr
        }
    }
}

/// A fresh 256-bit CSPRNG launch nonce, lowercase hex. Used to bind a reserved
/// module's registration to the exact process the supervisor spawned.
fn generate_launch_nonce() -> Result<String, SuperviseError> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|source| SuperviseError::LaunchNonce {
        reason: source.to_string(),
    })?;
    let mut hex = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    Ok(hex)
}

/// The kernel's image after an acknowledged exec, shared by ordinary launches
/// and privacy trampolines. PATH and shebang interpretation are kernel facts,
/// not identities inferred from a configured pathname.
#[cfg(not(windows))]
pub(super) fn observe_spawned_image(pid: u32) -> Option<subc_os::Observation> {
    let process = subc_os::Process::open(pid).ok().flatten()?;
    #[cfg(target_os = "linux")]
    return sample_after_linux_exec(
        || {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let committed = linux_exec_committed(&stat);
            #[cfg(test)]
            spawn_image_tests::report_exec_state(committed);
            committed
        },
        || process.observe(),
    );
    #[cfg(not(target_os = "linux"))]
    process.observe()
}

/// glibc's posix_spawn parent can wake in exec_mmap before the child's new mm
/// is installed. PF_FORKNOEXEC is cleared later in begin_new_exec, after that
/// installation. Until it clears, /proc/pid/exe can still name the daemon.
#[cfg(target_os = "linux")]
pub(super) fn linux_exec_committed(stat: &str) -> Option<bool> {
    const PF_FORKNOEXEC: u64 = 0x40;
    let flags = stat.rsplit_once(')')?.1.split_whitespace().nth(6)?;
    Some(flags.parse::<u64>().ok()? & PF_FORKNOEXEC == 0)
}

#[cfg(target_os = "linux")]
pub(super) fn sample_after_linux_exec(
    mut exec_committed: impl FnMut() -> Option<bool>,
    observe: impl FnOnce() -> Option<subc_os::Observation>,
) -> Option<subc_os::Observation> {
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while !exec_committed()? {
        // A failed or indefinitely stalled exec must not publish the inherited
        // image as authority to signal a future orphan. Missing identity keeps
        // the existing conservative orphan-sweep behavior.
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_micros(50));
    }
    observe()
}

/// Resolve once and spawn that exact path, so PATH lookup cannot choose a
/// different executable after the no-delete capture was taken.
#[cfg(windows)]
pub(crate) fn windows_spawn_capture(spec: &ModuleSpec) -> io::Result<subc_os::ExecutableCapture> {
    if let Err(error) = require_program_search_path(spec) {
        return Err(match error {
            SuperviseError::Spawn { source, .. } => source,
            error => io::Error::other(error),
        });
    }
    let mut program = spec.program.clone();
    if program.is_absolute() || program.components().count() > 1 {
        // An explicit path can name a PE image with no extension. Try that
        // exact file first, as Command does, before falling back to .exe.
        match subc_os::ExecutableCapture::open(&program) {
            Ok(capture) => return Ok(capture),
            Err(error)
                if error.kind() == io::ErrorKind::NotFound && program.extension().is_none() => {}
            Err(error) => return Err(error),
        }
        program.set_extension("exe");
        return subc_os::ExecutableCapture::open(&program);
    }
    if program.extension().is_none() {
        program.set_extension("exe");
    }
    let custom_path = spec
        .env
        .iter()
        .rev()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATH"));
    let mut directories = Vec::new();
    // These are Windows' default executable search locations. A configured
    // PATH still overrides the inherited PATH, not the system directories.
    if let Some(parent) = std::env::current_exe()?.parent() {
        directories.push(parent.to_path_buf());
    }
    directories.push(std::env::current_dir()?);
    if let Some(windows) = std::env::var_os("SystemRoot") {
        let windows = PathBuf::from(windows);
        directories.push(windows.join("System32"));
        directories.push(windows.clone());
    }
    let path = custom_path
        .map(|(_, value)| std::ffi::OsString::from(value))
        .or_else(|| std::env::var_os("PATH"));
    if let Some(path) = path {
        directories.extend(std::env::split_paths(&path));
    }
    for directory in directories {
        match subc_os::ExecutableCapture::open(&directory.join(&program)) {
            Ok(capture) => return Ok(capture),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "resolved executable not found",
    ))
}

#[cfg(test)]
pub(super) fn spawn_and_mark_running(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    snapshot: &SharedSnapshot,
) -> Result<SupervisedChild, SuperviseError> {
    let activity_spawn = runtime.begin_activity_spawn(&spec.module_id);
    let registration_watch = watch_registration(&runtime.registry, spec);
    #[cfg(unix)]
    let joining_group = containment::current(&runtime.child_roster.containment)
        .is_some_and(|container| container.group().is_some());
    let spawn = || {
        spawn_child(
            spec,
            runtime.connection_file_path.as_deref(),
            runtime.supervisor_handle.as_ref(),
            &runtime.stderr_ring,
            runtime.capture_logs_dir.as_deref(),
            &runtime.child_roster,
            runtime.spawn_delivery(),
            #[cfg(target_os = "linux")]
            runtime.cgroup_placement.as_ref(),
            runtime.register_timeout,
        )
    };
    let result = spawn();
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
            // The failed join already destroyed and verified the old container.
            // Try one fresh group as part of this same restart, without spending
            // another crash-budget entry. Cgroups and Windows jobs do not retry.
            warn!(module_id = %spec.module_id, %error, "carried process-group spawn failed; trying one fresh container");
            spawn()
        }
        other => other,
    };
    let child = result?;
    let mut child = child;
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

pub(super) fn set_running(
    snapshot: &SharedSnapshot,
    child: &SupervisedChild,
    module_id: &str,
    spawn_events: &SpawnEventFeed,
) -> Result<(), SuperviseError> {
    let mut state = snapshot.lock().map_err(|_| SuperviseError::StatePoisoned {
        module_id: Some(module_id.to_string()),
    })?;
    state.spawn_generation = spawn_events.emit_spawned(
        module_id,
        child
            .registration_spawn
            .as_ref()
            .map(|spawn| spawn.authority.spawn_number()),
        child.pid,
        child.spawned_at_ms,
    );
    if std::mem::take(&mut state.coalesced_restart_pending) {
        let generation = state.spawn_generation;
        info!(module_id, "restart already satisfied by generation {generation}; coalesced pending request completed");
    }
    state.drain_disposition_detail = None;
    state.spawn_failure = None;
    state.admission_refused = None;
    state.config_hold_parked = false;
    state.config_hold_daemon_initiated = false;
    state.config_hold_approval = None;
    state.legacy_registration_spawn =
        child.registration_spawn.is_some() && child.admission_base.is_none();
    state.registration_declaration = child.registration_declaration.clone();
    state.stop_reason = None;
    state.config_exit_failed_approval = None;
    state.waiting_for_previous_pid = None;
    state.waiting_for_previous_detail = None;
    // Every caller of this is a plain spawn, which always uses the primary key;
    // a promoted swap candidate sets the flag itself after this returns.
    state.in_alternate_slot = false;
    state.configuration_updated_since_spawn = false;
    state.spawned_protocol = Some(child.protocol);
    state.state = ModuleState::Running;
    state.approved_start_failure = None;
    state.enabled = true;
    state.process_alive = true;
    state.pid = child.id();
    state.subtree_target = child.subtree_target.clone();
    // Probe samples belong to one process. Keep module-level escalation and
    // late-answer history, but never charge a new child for its predecessor's misses.
    state.health.status = SupervisorHealthStatus::Unknown;
    state.last_probe_evidence = None;
    state.health.consecutive_failures = 0;
    state.health.detail = None;
    state.health.metrics = None;
    state.health.last_probe_ms = None;
    state.failed_retry.healthy_since = None;
    state.failed_retry.failed_at = None;
    state.failed_retry.running_since = Some(child.spawned_at);
    #[cfg(windows)]
    {
        state.pipe_consumed = child
            .nonce_delivery
            .as_ref()
            .map(|delivery| delivery.consumed());
    }
    #[cfg(target_os = "macos")]
    {
        state.report_ready = Some(Arc::clone(&child.report_ready));
    }
    state.spawned_at_ms = Some(child.spawned_at_ms);
    state.exec_outcome = Some(Arc::clone(&child.exec.0));
    state.spawned_from = Some(child.spawned_from.clone());
    state.launch_spec_digest = Some(child.launch_spec_digest.clone());
    state.spawned_file_identity = child.spawned_file_identity.clone();
    state.process_start_time = child.process_start_time;
    Ok(())
}
