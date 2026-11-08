//! Isolated development hooks. All hooks share the same executable-name check;
//! no environment-based override exists in a build without `test-support`.

pub use super::disk::PausePoint;
use super::*;
use std::fs;

#[derive(Clone)]
pub struct TestSupport {
    root: PathBuf,
    key_file: Option<PathBuf>,
    pause_at: Option<(PausePoint, PathBuf)>,
    fail_anchor: bool,
    deny_key: bool,
}
impl TestSupport {
    pub fn new(root: PathBuf, key_file: Option<PathBuf>) -> io::Result<Self> {
        authorize(&std::env::current_exe()?)?;
        StoreRoot::from_service_definition(root.clone())?;
        if key_file.as_ref().is_some_and(|p| !p.starts_with(&root)) {
            return Err(io::Error::other(
                "test key must live under isolated store root",
            ));
        }
        Ok(Self {
            root,
            key_file,
            pause_at: None,
            fail_anchor: false,
            deny_key: false,
        })
    }
    pub fn with_anchor_failure(mut self, fail: bool) -> Self {
        self.fail_anchor = fail;
        self
    }
    pub fn with_key_denial(mut self, denied: bool) -> Self {
        self.deny_key = denied;
        self
    }
    /// Writes `<path>.reached`, then waits for `<path>.release`. The rig owns
    /// these files and must kill/release the child; there is no timeout race.
    pub fn with_pause(mut self, point: PausePoint, path: PathBuf) -> Self {
        self.pause_at = Some((point, path));
        self
    }
    pub fn open(self) -> io::Result<ConfigStore> {
        disk::private_directory(&self.root)?;
        let backend = if let Some(path) = &self.key_file {
            let mut keys = subc_os::config_keychain::testing::FileKeyStore::open(path)
                .map_err(io::Error::other)?;
            keys.set_update_failure(self.fail_anchor);
            keys.set_access_denied(self.deny_key);
            Backend::File(keys)
        } else {
            Backend::Unsupported
        };
        let mut store = ConfigStore::load(StoreRoot(self.root.clone()), backend);
        store.hooks = Some(self);
        Ok(store)
    }
    pub fn pause(&self, point: PausePoint) -> io::Result<()> {
        if let Some((configured, path)) = &self.pause_at {
            if *configured == point {
                fs::write(path.with_extension("reached"), b"ready")?;
                while !path.with_extension("release").exists() {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
        Ok(())
    }
    pub fn pause_spawn_before_exec(&self) -> io::Result<()> {
        self.pause(PausePoint::SpawnBeforeExec)
    }
    pub fn pause_swap_before_promotion(&self) -> io::Result<()> {
        self.pause(PausePoint::SwapBeforePromotion)
    }
}

fn authorize(executable: &Path) -> io::Result<()> {
    if !executable
        .file_name()
        .and_then(|p| p.to_str())
        .is_some_and(|name| name.starts_with("ckdev-"))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "test store hooks require ckdev- executable",
        ));
    }
    Ok(())
}

impl ConfigStore {
    pub fn test_support(&self) -> Option<&TestSupport> {
        self.hooks.as_ref()
    }
    /// Changing the failure flag never bypasses a pending anchor; retry remains
    /// explicit so the delivery caller cannot mistake a flag change for a commit.
    pub fn set_test_anchor_failure(&mut self, fail: bool) -> io::Result<()> {
        if self.hooks.is_none() {
            return Err(io::Error::other("test hooks not enabled"));
        }
        if let Backend::File(keys) = &mut self.backend {
            keys.set_update_failure(fail);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn config_store_test_seams_reject_non_ckdev_executable() {
        assert!(authorize(Path::new("subc")).is_err());
        assert!(authorize(Path::new("ckdev-store")).is_ok());
        assert!(TestSupport::new(std::env::current_dir().unwrap(), None).is_err());
    }
}
