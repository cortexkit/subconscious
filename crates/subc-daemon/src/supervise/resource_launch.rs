use super::{SupervisedModule, SupervisorHandle, SupervisorSnapshot};

#[cfg(test)]
pub(super) type ResourceCommitPause = (
    tokio::sync::oneshot::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResourceCommitError {
    Busy,
    Changed,
}

#[derive(Debug, Clone)]
pub(crate) struct ResourceLaunch {
    pub pid: Option<u32>,
    pub start: Option<u64>,
    pub spawn_generation: u64,
    pub enabled: bool,
    pub alive: bool,
    pub target: crate::subtree_resources::Target,
}

impl ResourceLaunch {
    fn from_snapshot(snapshot: &SupervisorSnapshot) -> Self {
        Self {
            pid: snapshot.reported_pid(),
            start: snapshot.process_start_time,
            spawn_generation: snapshot.spawn_generation,
            enabled: snapshot.enabled,
            alive: snapshot.process_alive,
            target: snapshot.subtree_target.clone(),
        }
    }

    pub(crate) fn generation(&self) -> u128 {
        (u128::from(self.spawn_generation) << 64) | u128::from(self.start.unwrap_or(0))
    }

    fn same_launch(&self, other: &Self) -> bool {
        self.pid == other.pid
            && self.start == other.start
            && self.spawn_generation == other.spawn_generation
            && self.enabled == other.enabled
            && self.alive == other.alive
    }
}

impl SupervisedModule {
    #[cfg(test)]
    pub(crate) fn with_resource_snapshot_locked_for_test<T>(&self, work: impl FnOnce() -> T) -> T {
        let _snapshot = self.inner.snapshot.lock().unwrap();
        work()
    }
    pub(crate) fn resource_launch(&self) -> Option<ResourceLaunch> {
        let snapshot = self.inner.snapshot.try_lock().ok()?;
        Some(ResourceLaunch::from_snapshot(&snapshot))
    }
}

impl SupervisorHandle {
    #[cfg(all(test, unix))]
    pub(crate) fn pause_resource_commit_for_test(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (reached, reached_rx) = tokio::sync::oneshot::channel();
        let (resume, resume_rx) = std::sync::mpsc::channel();
        *self.resource_commit_pause.lock().unwrap() = Some((reached, resume_rx));
        (reached_rx, resume)
    }
    /// Keep launch replacement and nonce rotation excluded only for the short,
    /// syscall-free accounting commit, never while validating processes.
    pub(crate) fn with_current_resource_launch<T>(
        &self,
        id: &str,
        nonce: &str,
        launch: &ResourceLaunch,
        commit: impl FnOnce() -> T,
    ) -> Result<T, ResourceCommitError> {
        #[cfg(test)]
        if let Some((reached, resume)) = self.resource_commit_pause.lock().unwrap().take() {
            let _ = reached.send(());
            let _ = resume.recv_timeout(std::time::Duration::from_secs(2));
        }
        let runtime = self
            .gate_runtime
            .try_lock()
            .map_err(|_| ResourceCommitError::Busy)?
            .clone();
        if let Some(runtime) = runtime {
            let owner = runtime.upgrade().ok_or(ResourceCommitError::Changed)?;
            return owner
                .with_legacy_nonce_authority(id, || {
                    self.commit_current_resource_launch(id, nonce, launch, commit)
                })
                .map_err(|()| ResourceCommitError::Busy)?
                .ok_or(ResourceCommitError::Changed)?;
        }
        self.commit_current_resource_launch(id, nonce, launch, commit)
    }

    /// The commit fence for a declaration made on a registration channel, which
    /// carries no launch nonce. Its checks and `commit` run under three locks
    /// held together: the spawn's transition guard, then the roster (`modules`),
    /// then the module's launch snapshot. Retirement takes the transition guard,
    /// so a spawn that exits after the checks retires only after the commit
    /// returns. Cutover rewrites the snapshot with the candidate's process, so
    /// a spawn superseded after the checks cannot commit either. The roster and
    /// snapshot are try-locked in the same order as the nonce fence, and
    /// contention reports `Busy`. The registry check under them takes only the
    /// registry's own lock, which is never held while another lock is taken.
    pub(crate) fn with_current_channel_resource_launch<T>(
        &self,
        registry: &crate::Registry,
        connection: crate::ConnectionId,
        spawn: &super::RegistrationSpawn,
        launch: &ResourceLaunch,
        commit: impl FnOnce() -> T,
    ) -> Result<T, ResourceCommitError> {
        #[cfg(test)]
        if let Some((reached, resume)) = self.resource_commit_pause.lock().unwrap().take() {
            let _ = reached.send(());
            let _ = resume.recv_timeout(std::time::Duration::from_secs(2));
        }
        let id = spawn.module_id.as_str();
        // A gated module's channel must carry a verified program. Gating changes
        // only when the configuration gate is stamped or unstamped, never with
        // this launch, so read it before taking any lock.
        if self.module_is_gated(id) && !spawn.has_program_binding() {
            return Err(ResourceCommitError::Changed);
        }
        let _alive = spawn
            .begin_publication()
            .map_err(|_| ResourceCommitError::Changed)?;
        let modules = self
            .modules
            .try_lock()
            .map_err(|_| ResourceCommitError::Busy)?;
        let module = modules.get(id).ok_or(ResourceCommitError::Changed)?;
        let snapshot = module
            .inner
            .snapshot
            .try_lock()
            .map_err(|_| ResourceCommitError::Busy)?;
        let current = ResourceLaunch::from_snapshot(&snapshot);
        let serving = registry
            .get_module(id)
            .ok()
            .flatten()
            .is_some_and(|registration| registration.connection_id == connection);
        let same_process = spawn.identity.get().is_some_and(|identity| {
            current.pid == Some(identity.pid) && current.start == Some(identity.start_time)
        });
        if !serving || !same_process || !launch.same_launch(&current) {
            return Err(ResourceCommitError::Changed);
        }
        Ok(commit())
    }

    fn commit_current_resource_launch<T>(
        &self,
        id: &str,
        nonce: &str,
        launch: &ResourceLaunch,
        commit: impl FnOnce() -> T,
    ) -> Result<T, ResourceCommitError> {
        let modules = self
            .modules
            .try_lock()
            .map_err(|_| ResourceCommitError::Busy)?;
        let nonces = self
            .spawn_nonces
            .try_lock()
            .map_err(|_| ResourceCommitError::Busy)?;
        if nonces.get(id).map(String::as_str) != Some(nonce) {
            return Err(ResourceCommitError::Changed);
        }
        let module = modules.get(id).ok_or(ResourceCommitError::Changed)?;
        let snapshot = module
            .inner
            .snapshot
            .try_lock()
            .map_err(|_| ResourceCommitError::Busy)?;
        if !launch.same_launch(&ResourceLaunch::from_snapshot(&snapshot)) {
            return Err(ResourceCommitError::Changed);
        }
        Ok(commit())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn launch_snapshot_is_atomic_and_a_stale_declaration_cannot_commit() {
        let handle = SupervisorHandle::new();
        let supervisor = super::super::Supervisor::new_for_test(
            std::sync::Arc::new(crate::Registry::default()),
            super::super::RestartPolicy::default(),
        )
        .with_handle(handle.clone());
        let module = supervisor
            .supervise_configured(
                crate::ModuleSpec {
                    module_id: "resource-launch-test".into(),
                    program: std::env::current_exe().unwrap(),
                    args: Vec::new(),
                    env: Vec::new(),
                    reserved: false,
                    reserved_prefixes: Vec::new(),
                    protocol: subc_control::ModuleProtocol::None,
                    overlap: Default::default(),
                    carry_containment: false,
                },
                false,
            )
            .unwrap();
        {
            let mut snapshot = module.inner.snapshot.lock().unwrap();
            snapshot.pid = Some(42);
            snapshot.process_start_time = Some(100);
            snapshot.spawn_generation = 7;
            snapshot.enabled = true;
            snapshot.process_alive = true;
            assert!(
                module.resource_launch().is_none(),
                "contention cannot be mistaken for a missing container"
            );
        }
        let launch = module.resource_launch().unwrap();
        assert_eq!(launch.generation(), (7_u128 << 64) | 100);
        let id = module.module_id();
        handle
            .spawn_nonces
            .lock()
            .unwrap()
            .insert(id.into(), "nonce".into());
        assert_eq!(
            handle.with_current_resource_launch(id, "nonce", &launch, || 123),
            Ok(123)
        );
        macro_rules! assert_busy {
            ($lock:expr) => {{
                let guard = $lock;
                assert_eq!(
                    handle.with_current_resource_launch(id, "nonce", &launch, || {
                        panic!("a contended commit fence must not run the mutation")
                    }),
                    Err::<(), _>(ResourceCommitError::Busy)
                );
                drop(guard);
            }};
        }
        assert_busy!(handle.modules.lock().unwrap());
        assert_busy!(handle.spawn_nonces.lock().unwrap());
        assert_busy!(module.inner.snapshot.lock().unwrap());
        module.inner.snapshot.lock().unwrap().spawn_generation += 1;
        assert!(handle
            .with_current_resource_launch(id, "nonce", &launch, || panic!(
                "stale generation committed"
            ))
            .is_err_and(|error| error == ResourceCommitError::Changed));
        let current = module.resource_launch().unwrap();
        handle
            .spawn_nonces
            .lock()
            .unwrap()
            .insert(id.into(), "replacement".into());
        assert!(handle
            .with_current_resource_launch(id, "nonce", &current, || panic!("stale nonce committed"))
            .is_err_and(|error| error == ResourceCommitError::Changed));
    }

    #[tokio::test]
    async fn channel_fence_commits_only_for_the_live_serving_spawn() {
        use super::super::{ProcessIdentity, RegistrationSpawn, SpawnRole};
        let handle = SupervisorHandle::new();
        let supervisor = super::super::Supervisor::new_for_test(
            std::sync::Arc::new(crate::Registry::default()),
            super::super::RestartPolicy::default(),
        )
        .with_handle(handle.clone());
        let module = supervisor
            .supervise_configured(
                crate::ModuleSpec {
                    module_id: "channel-fence-test".into(),
                    program: std::env::current_exe().unwrap(),
                    args: Vec::new(),
                    env: Vec::new(),
                    reserved: false,
                    reserved_prefixes: Vec::new(),
                    protocol: subc_control::ModuleProtocol::None,
                    overlap: Default::default(),
                    carry_containment: false,
                },
                false,
            )
            .unwrap();
        let set_process = |pid: u32, start: u64| {
            let mut snapshot = module.inner.snapshot.lock().unwrap();
            snapshot.pid = Some(pid);
            snapshot.process_start_time = Some(start);
            snapshot.spawn_generation = 7;
            snapshot.enabled = true;
            snapshot.process_alive = true;
        };
        set_process(42, 100);
        let id = module.module_id();
        let registry = crate::Registry::default();
        let serving = crate::ConnectionId::new(5);
        registry
            .register_with_control_ops(
                subc_protocol::manifest::ModuleManifest::builder(id, "1.0.0").build(),
                subc_protocol::PROTOCOL_VERSION,
                serving,
                Vec::new(),
            )
            .unwrap();
        let live_spawn = |pid: u32, start: u64| {
            let spawn = RegistrationSpawn::new(&handle, id, SpawnRole::Plain, None);
            spawn
                .identity
                .set(ProcessIdentity {
                    pid,
                    start_time: start,
                })
                .unwrap();
            spawn.ready();
            spawn
        };
        let spawn = live_spawn(42, 100);
        let launch = module.resource_launch().unwrap();
        let fence = |connection, spawn: &RegistrationSpawn, launch: &ResourceLaunch| {
            handle.with_current_channel_resource_launch(&registry, connection, spawn, launch, || 1)
        };
        assert_eq!(fence(serving, &spawn, &launch), Ok(1));
        assert_eq!(
            fence(crate::ConnectionId::new(6), &spawn, &launch),
            Err(ResourceCommitError::Changed),
            "a connection the registry does not name as serving cannot commit"
        );

        // Retirement waits for a commit already past its checks.
        let retiring = spawn.clone();
        let retirement = handle
            .with_current_channel_resource_launch(&registry, serving, &spawn, &launch, || {
                let retirement = std::thread::spawn(move || retiring.retire());
                std::thread::sleep(std::time::Duration::from_millis(50));
                assert!(spawn.launch_ready(), "retirement ran inside the commit");
                retirement
            })
            .unwrap();
        retirement.join().unwrap();
        assert!(!spawn.launch_ready());
        assert_eq!(
            fence(serving, &spawn, &launch),
            Err(ResourceCommitError::Changed),
            "a retired spawn cannot commit"
        );

        // Cutover rewrites the launch record with the candidate's process: the
        // superseded spawn cannot commit even against the current record.
        let superseded = live_spawn(42, 100);
        set_process(43, 200);
        let current = module.resource_launch().unwrap();
        assert_eq!(
            fence(serving, &superseded, &current),
            Err(ResourceCommitError::Changed)
        );
        assert_eq!(fence(serving, &live_spawn(43, 200), &current), Ok(1));

        // Contention is reported as busy, never as a changed launch.
        let fresh = live_spawn(43, 200);
        let _snapshot = module.inner.snapshot.lock().unwrap();
        assert_eq!(
            fence(serving, &fresh, &current),
            Err(ResourceCommitError::Busy)
        );
    }
}
