use super::*;
use crate::daemon_config::{ActivityConfig, DaemonConfig};

#[derive(Clone)]
pub(crate) enum ApprovedDaemonView {
    NotStamped,
    Unavailable,
    Approved {
        generation: u64,
        config: std::sync::Arc<DaemonConfig>,
    },
}

impl ApprovedDaemonView {
    pub(super) fn from_store(store: &ConfigStore, recovering: bool, previous: &Self) -> Self {
        let (state, body) = store.loaded_admission_state();
        if recovering || store.loaded_anchor_pending() {
            return Self::Unavailable;
        }
        match state {
            StoreState::Unstamped => return Self::NotStamped,
            StoreState::StoreMissing | StoreState::StoreUnverified => return Self::Unavailable,
            StoreState::Stamped => {}
        }
        if matches!(previous, Self::Approved { generation, .. } if *generation == body.last_generation)
        {
            return previous.clone();
        }
        body.generations
            .get(&body.last_generation)
            .and_then(|entry| entry.delivery.as_ref())
            .and_then(|snapshot| parse_daemon(snapshot).ok())
            .map(|config| Self::Approved {
                generation: body.last_generation,
                config: std::sync::Arc::new(config),
            })
            .unwrap_or(Self::Unavailable)
    }
}

fn parse_daemon(snapshot: &DeliverySnapshot) -> Result<DaemonConfig> {
    let layer = snapshot
        .daemon
        .as_ref()
        .ok_or_else(|| GateError::new("config_gated", "approved daemon configuration is absent"))?;
    let bytes = layer.bytes.as_deref().ok_or_else(GateError::stale)?;
    let path = layer.path.as_deref().ok_or_else(GateError::stale)?;
    let doc = std::str::from_utf8(bytes).map_err(GateError::invalid)?;
    crate::daemon_config::parse_with_activity(doc, std::path::Path::new(path))
        .map(|(config, _): (DaemonConfig, ActivityConfig)| config)
        .map_err(|error| GateError::new("invalid_daemon_config", error.to_string()))
}

impl RuntimeOwner {
    /// Test readers inspect the published approved daemon view without consulting the store.
    #[cfg(test)]
    pub(crate) fn approved_daemon_view(&self) -> ApprovedDaemonView {
        if self.inner.is_poisoned() {
            return ApprovedDaemonView::Unavailable;
        }
        self.published
            .read()
            .map(|view| view.approved_daemon.clone())
            .unwrap_or(ApprovedDaemonView::Unavailable)
    }

    /// A file snapshot is proposal data only. Candidate previews must not call
    /// this path because their text belongs to the client, not the filesystem.
    pub(crate) fn observe_file_proposal(&self) -> Result<()> {
        let mut state = self.lock()?;
        Self::check_authority(&mut state, false)?;
        if !state.recovery_exit_hold && !state.store.status().anchor_pending {
            let captured = self.capture(&mut state)?;
            state.store.observe(captured.diagnostic.to_store()?)?;
        }
        Ok(())
    }

    pub(crate) fn approved_daemon_configuration(&self) -> Result<Option<DaemonConfig>> {
        let mut state = self.lock()?;
        Self::check_authority(&mut state, false)?;
        if state.store.status().state != StoreState::Stamped {
            return Ok(None);
        }
        state.store.delivery_ready()?;
        state
            .store
            .body()?
            .generations
            .values()
            .next_back()
            .and_then(|entry| entry.delivery.as_ref())
            .ok_or_else(|| GateError::new("config_gated", "approved daemon snapshot is absent"))
            .and_then(parse_daemon)
            .map(Some)
    }

    pub(crate) fn generation_daemon_configurations(
        &self,
        generation: u64,
    ) -> Result<Option<(DaemonConfig, DaemonConfig)>> {
        let mut state = self.lock()?;
        Self::check_authority(&mut state, false)?;
        let body = state.store.body()?;
        let Some(next) = body.generations.get(&generation) else {
            return Err(GateError::stale());
        };
        if next.kind == crate::config_store::CommitKind::Stamp {
            return Ok(None);
        }
        let Some(previous) = body
            .generations
            .range(..generation)
            .next_back()
            .and_then(|(_, entry)| entry.delivery.as_ref())
        else {
            return Ok(None);
        };
        let next = next.delivery.as_ref().ok_or_else(GateError::stale)?;
        Ok(Some((parse_daemon(previous)?, parse_daemon(next)?)))
    }
}
