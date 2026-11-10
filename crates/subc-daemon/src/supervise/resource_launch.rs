use super::{SupervisedModule, SupervisorHandle, SupervisorSnapshot};

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
    /// Keep launch replacement and nonce rotation excluded only for the short,
    /// syscall-free accounting commit, never while validating processes.
    pub(crate) fn with_current_resource_launch<T>(
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
}
