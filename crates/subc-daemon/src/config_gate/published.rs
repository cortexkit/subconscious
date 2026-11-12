//! Admission, status and approved daemon configuration share one publication.
use super::*;

pub(crate) struct Published {
    pub(super) admission: admission::AdmissionCache,
    pub(crate) approved_daemon: ApprovedDaemonView,
    pub(super) projection: polled::Projection,
}

impl Published {
    pub(crate) fn is_recovering(&self) -> bool {
        self.admission.is_recovering()
    }
}

impl std::ops::Deref for Published {
    type Target = admission::AdmissionCache;
    fn deref(&self) -> &Self::Target {
        &self.admission
    }
}

impl RuntimeOwner {
    pub(crate) fn polled_snapshot(&self) -> Result<std::sync::Arc<Published>> {
        if self.inner.is_poisoned() {
            return Err(GateError::new(
                "config_unavailable",
                "configuration owner lock is poisoned",
            ));
        }
        let snapshot = self
            .published
            .read()
            .map(|value| value.clone())
            .map_err(|_| {
                GateError::new(
                    "config_unavailable",
                    "configuration publication is unavailable",
                )
            })?;
        #[cfg(test)]
        if let Some(hook) = self.polled_capture_hook.lock().unwrap().take() {
            hook();
        }
        Ok(snapshot)
    }
}
