//! File-backed sealing for isolated development daemons. This module is absent
//! without `test-support`; opening it also requires a `ckdev-` executable name.
//! It models missing, denied and failed-anchor writes, not macOS ACL behavior.

use super::{AnchorState, KeychainError, KeychainState, StoreAnchor, StoreKey};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

/// The test rig owns the isolated directory and serializes writes. Neither
/// flags nor paths are read from the environment by this primitive.
pub struct FileKeyStore {
    path: PathBuf,
    denied: bool,
    fail_updates: bool,
}

impl FileKeyStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, KeychainError> {
        let executable = std::env::current_exe().map_err(KeychainError::Io)?;
        Self::open_for_executable(path.into(), &executable)
    }

    pub(super) fn open_for_executable(
        path: PathBuf,
        executable: &Path,
    ) -> Result<Self, KeychainError> {
        if !executable
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("ckdev-"))
        {
            return Err(KeychainError::Unsupported);
        }
        Ok(Self {
            path,
            denied: false,
            fail_updates: false,
        })
    }

    pub fn set_access_denied(&mut self, denied: bool) {
        self.denied = denied;
    }
    pub fn set_update_failure(&mut self, fail: bool) {
        self.fail_updates = fail;
    }

    fn access(&self) -> Result<(), KeychainError> {
        if self.denied {
            Err(KeychainError::Denied)
        } else {
            Ok(())
        }
    }

    pub fn load(&self) -> Result<Option<KeychainState>, KeychainError> {
        self.access()?;
        match fs::read(&self.path) {
            Ok(bytes) => KeychainState::decode(&Zeroizing::new(bytes)).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(KeychainError::Io(error)),
        }
    }

    pub fn create(&self, key: StoreKey) -> Result<(), KeychainError> {
        self.access()?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&self.path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                KeychainError::AlreadyExists
            } else {
                KeychainError::Io(error)
            }
        })?;
        let bytes = KeychainState {
            key,
            anchor: AnchorState::default(),
        }
        .encode();
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(KeychainError::Io)
    }

    pub fn update_anchor(&self, anchor: StoreAnchor) -> Result<(), KeychainError> {
        self.change_anchor(|old| old.update(anchor))
    }

    pub fn clear_anchor(&self, unstamp_revision: u64) -> Result<(), KeychainError> {
        self.change_anchor(|old| old.clear(unstamp_revision))
    }

    fn change_anchor(
        &self,
        change: impl FnOnce(AnchorState) -> Result<AnchorState, KeychainError>,
    ) -> Result<(), KeychainError> {
        let mut state = self.load()?.ok_or(KeychainError::Missing)?;
        state.anchor = change(state.anchor)?;
        if self.fail_updates {
            return Err(KeychainError::Unavailable);
        }
        // One writer per rig: the pending file is never the authoritative item.
        let pending = self.path.with_extension("pending");
        let result = (|| {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&pending)?;
            file.write_all(&state.encode())?;
            file.sync_all()?;
            fs::rename(&pending, &self.path)?;
            #[cfg(unix)]
            fs::File::open(self.path.parent().unwrap_or(Path::new(".")))?.sync_all()?;
            Ok::<_, std::io::Error>(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&pending);
        }
        result.map_err(KeychainError::Io)
    }

    pub fn delete(&self) -> Result<(), KeychainError> {
        self.access()?;
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(KeychainError::Missing)
            }
            Err(error) => Err(KeychainError::Io(error)),
        }
    }
}
