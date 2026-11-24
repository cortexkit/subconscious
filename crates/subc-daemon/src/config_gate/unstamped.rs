//! Observe live user files without holding a module gate or the runtime owner.

use super::*;
use crate::config_gate::capture::{SettingsCapture, UserFileMetadata};
use std::path::Path;
use subc_config::Digest;

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
    pub(super) fn log_user_file_observations(&self, captured: &SettingsCapture) {
        for (module, accepted) in &captured.configuration.modules {
            self.log_user_file_observation(module, &accepted.file.path, None);
        }
        for (module, digest) in &captured.refused_file_digests {
            let error = &captured.file_errors[module];
            if let Some(path) = &error.path {
                self.log_user_file_observation(
                    module,
                    Path::new(path),
                    Some((digest, &error.code, &error.message)),
                );
            }
        }
    }

    fn log_user_file_observation(
        &self,
        module: &str,
        path: &Path,
        refusal: Option<(&Digest, &str, &str)>,
    ) {
        // Each module has one canonical user settings path. Sharing
        // the digest across observers also avoids repeat logs on reconnect.
        let Ok(mut refused) = self.user_file_refusals.lock() else {
            return;
        };
        match refusal {
            Some((digest, code, reason)) if refused.get(module) != Some(digest) => {
                refused.insert(module.to_owned(), digest.clone());
                tracing::warn!(module_id = %module, path = %path.display(), %code, %reason,
                    "user settings file refused");
            }
            None if refused.remove(module).is_some() => {
                tracing::info!(module_id = %module, path = %path.display(),
                    "user settings file accepted after refusal");
            }
            _ => {}
        }
    }

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
        let path = Path::new(&declaration.user_file);
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
        let view = self
            .capturer
            .capture_module_settings(&declarations, &BTreeSet::from([module.to_owned()]))
            .and_then(|mut captured| {
                captured
                    .modules
                    .remove(module)
                    .map(|module| module.view)
                    .ok_or_else(|| {
                        GateError::new("config_gated", "module declaration is unavailable")
                    })
            });
        let refusal = match &view {
            Ok(_) => None,
            Err(error) => {
                let Some(digest) = error
                    .settings_file
                    .as_ref()
                    .and_then(|file| file.content_digest.as_ref())
                else {
                    return Err(error.clone());
                };
                if metadata.as_ref().and_then(UserFileMetadata::content_digest) != Some(digest) {
                    return Ok(UserFilePoll::Observed);
                }
                Some((digest, error.code.as_str(), error.detail.as_str()))
            }
        };
        // Do not cache a fingerprint for bytes read across an atomic replacement.
        if UserFileMetadata::read(path)? != metadata {
            return Ok(UserFilePoll::Observed);
        }
        let mut state = self.lock()?;
        if state.store.loaded_admission_state().0 != StoreState::Unstamped
            || state.authority_exit.is_some()
            || !std::sync::Arc::ptr_eq(&incarnation, &state.authority_incarnation)
            || !state
                .declarations
                .get(&connection)
                .is_some_and(|(id, declared)| id == module && declared == &declaration)
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
        self.log_user_file_observation(module, path, refusal);
        if view.is_ok_and(|view| served.as_ref().is_some_and(|served| **served != view)) {
            state.unstamp_hints.insert(connection);
            self.notify_delivery_changed();
        }
        Ok(UserFilePoll::Observed)
    }
}
