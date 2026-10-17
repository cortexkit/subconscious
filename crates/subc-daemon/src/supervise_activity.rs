use super::*;

#[derive(Debug, Clone)]
pub(super) struct ActivityImage {
    image: Arc<crate::activity::RegistrationImage>,
    requester: Option<Principal>,
}

pub(super) struct ActivitySpawn(ActivityImage);

impl ActivitySpawn {
    pub(super) fn complete(self, snapshot: &SharedSnapshot) {
        let facts = lock_snapshot(snapshot).ok().and_then(|state| {
            Some(crate::activity::ImageFacts {
                pid: state.pid,
                path: state.spawned_from.clone(),
                identity: state.spawned_file_identity.clone(),
                start: state.process_start_time,
                exec: state.exec_outcome.clone()?,
            })
        });
        self.0.image.complete(facts);
    }
}

impl Drop for ActivitySpawn {
    fn drop(&mut self) {
        // A HELLO can race spawn bookkeeping. Even a failed spawn must release
        // the writer's pending observation rather than strand its queue.
        self.0.image.complete(None);
    }
}

impl SupervisorHandle {
    pub(crate) fn set_activity(&self, ledger: Option<crate::activity::ActivityLedger>) {
        *self.activity.lock().unwrap_or_else(|p| p.into_inner()) = ledger;
    }

    pub(crate) fn activity(&self) -> Option<crate::activity::ActivityLedger> {
        self.activity
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub(crate) fn record_activity_registration(&self, id: &str) {
        let Some(ledger) = self.activity() else {
            return;
        };
        let image = self
            .activity_images
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .cloned();
        match image {
            Some(image) => ledger.module_registered(id, image.requester, image.image),
            None => ledger.skip_start_digest(),
        }
    }
}

impl SupervisorRuntimeConfig {
    pub(super) fn record_activity_transition(
        &self,
        kind: &str,
        id: &str,
        requester: Option<Principal>,
    ) {
        if let (Some(ledger), Some(requester)) = (
            self.supervisor_handle
                .as_ref()
                .and_then(SupervisorHandle::activity),
            requester,
        ) {
            ledger.module_transition(kind, id, requester);
        }
    }

    pub(super) fn begin_activity_spawn(&self, id: &str) -> Option<ActivitySpawn> {
        let handle = self.supervisor_handle.as_ref()?;
        handle.activity()?;
        let image = ActivityImage {
            image: Arc::new(crate::activity::RegistrationImage::new(
                self.provenance_probe.clone(),
            )),
            requester: self.start_requester.clone(),
        };
        // Install an immutable per-spawn observation before the OS can run the
        // child. Every accepted HELLO queues that exact spawn's facts, including
        // a registration that disappears before the supervision loop wakes.
        handle
            .activity_images
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.to_owned(), image.clone());
        Some(ActivitySpawn(image))
    }
}
