//! Observe live user files without holding a module gate or the runtime owner.

use super::*;
use crate::config_gate::capture::UserFileMetadata;

pub(crate) const USER_FILE_POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(crate) struct UserFileWatch {
    // The outer None means no successful observation yet; the inner None is absence.
    metadata: Option<Option<UserFileMetadata>>,
    served: Option<std::sync::Arc<ConfigView>>,
}

/// What one poll of a connection's declared user file found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UserFilePoll {
    /// The file is not watched: the store is not unstamped, an authority exit
    /// is pending, or the connection has no matching declaration.
    Stopped,
    /// The file's metadata and the served view match the last observation.
    Unchanged,
    /// Something differed from the last observation, or it could not be
    /// compared. The file stays watched.
    Observed,
}

impl RuntimeOwner {
    /// Called on a blocking worker, before acquiring the module's delivery gate.
    /// Anything but `Stopped` means the connection still needs an unstamped
    /// polling timer. An unchanged file costs one plain read of the owner's state and one
    /// bounded file read. It takes no `OwnerGuard` (whose drop rebuilds the admission
    /// snapshot and wakes the delivery worker) and publishes nothing.
    pub(crate) fn poll_user_file(
        &self,
        connection: ConnectionId,
        module: &str,
        watch: &mut UserFileWatch,
    ) -> Result<UserFilePoll> {
        let (declaration, served, incarnation) = {
            // Only read here. Queued disconnects are not applied, so a closed
            // connection's declaration may still be seen; the guarded recheck
            // below runs before any hint is queued.
            crate::forwarding::assert_forwarding_unlocked("the configuration owner's state");
            let state = self.inner.lock().map_err(|_| {
                GateError::new("config_unavailable", "configuration owner lock is poisoned")
            })?;
            if state.store.loaded_admission_state().0 != StoreState::Unstamped
                || state.authority_exit.is_some()
            {
                *watch = UserFileWatch::default();
                return Ok(UserFilePoll::Stopped);
            }
            let Some((_, declaration)) = state
                .declarations
                .get(&connection)
                .filter(|(id, _)| id == module)
            else {
                *watch = UserFileWatch::default();
                return Ok(UserFilePoll::Stopped);
            };
            (
                declaration.clone(),
                state.delivery.unstamped_views.get(&connection).cloned(),
                state.authority_incarnation.clone(),
            )
        };
        let path = std::path::Path::new(&declaration.user_file);
        let metadata = UserFileMetadata::read(path)?;
        if metadata
            .as_ref()
            .is_some_and(|metadata| !metadata.has_content_digest())
        {
            return Ok(UserFilePoll::Observed);
        }
        let same_view = match (&watch.served, &served) {
            (Some(before), Some(after)) => std::sync::Arc::ptr_eq(before, after),
            (None, None) => true,
            _ => false,
        };
        if watch.metadata.as_ref() == Some(&metadata) && same_view {
            return Ok(UserFilePoll::Unchanged);
        }
        let declarations = BTreeMap::from([(module.to_owned(), declaration.clone())]);
        let mut captured = self
            .capturer
            .capture_module_settings(&declarations, &BTreeSet::from([module.to_owned()]))?;
        let view = captured
            .modules
            .remove(module)
            .ok_or_else(|| GateError::new("config_gated", "module declaration is unavailable"))?
            .view;
        // Do not cache a fingerprint for bytes read across an atomic replacement.
        if UserFileMetadata::read(path)? != metadata {
            return Ok(UserFilePoll::Observed);
        }
        let mut state = self.lock()?;
        if state.store.loaded_admission_state().0 != StoreState::Unstamped
            || state.authority_exit.is_some()
            || !std::sync::Arc::ptr_eq(&incarnation, &state.authority_incarnation)
            || state.declarations.get(&connection) != Some(&(module.to_owned(), declaration))
        {
            *watch = UserFileWatch::default();
            return Ok(UserFilePoll::Stopped);
        }
        let current = state.delivery.unstamped_views.get(&connection);
        let same_receipt = match (&served, current) {
            (Some(before), Some(after)) => std::sync::Arc::ptr_eq(before, after),
            (None, None) => true,
            _ => false,
        };
        if !same_receipt {
            // A config.get completed during the capture. Retry the comparison
            // against the view that get recorded as delivered.
            return Ok(UserFilePoll::Observed);
        }
        watch.metadata = Some(metadata);
        watch.served = served.clone();
        if served.as_ref().is_some_and(|served| **served != view) {
            state.unstamp_hints.insert(connection);
            self.notify_delivery_changed();
        }
        Ok(UserFilePoll::Observed)
    }
}
