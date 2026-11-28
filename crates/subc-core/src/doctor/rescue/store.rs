use std::{
    fs::{self, File, TryLockError},
    path::{Path, PathBuf},
};

use super::{
    files::{absent, at, existing_dir, lock_file, remove_entry, Directory, Stage},
    host::Host,
    pins,
};

pub(super) struct Lease<T> {
    pub value: T,
    pub lock: File,
}

struct InstallLock(File);

impl Drop for InstallLock {
    fn drop(&mut self) {
        // A concurrent fork can keep the descriptor open until exec, even with
        // close-on-exec set. Release the lock when the install is done.
        let _ = self.0.unlock();
    }
}

pub(super) struct Store {
    folder: Directory,
    pin: &'static str,
}

impl Store {
    pub fn new(folder: Directory, pin: &'static str) -> Self {
        Self { folder, pin }
    }

    pub fn path(&self) -> PathBuf {
        self.folder.path().join(self.pin)
    }

    fn use_file(&self, version: &str) -> Result<File, String> {
        lock_file(&self.folder, &format!(".use-{version}"))
    }

    fn shared(&self) -> Result<File, String> {
        let lock = self.use_file(self.pin)?;
        at(
            &self.folder.path().join(format!(".use-{}", self.pin)),
            lock.lock_shared(),
        )?;
        Ok(lock)
    }

    pub fn get<T>(
        &self,
        validate: impl FnOnce(&Path) -> Result<Option<T>, String>,
    ) -> Result<Option<Lease<T>>, String> {
        let lock = self.shared()?;
        let path = self.path();
        if !existing_dir(&path) {
            return Ok(None);
        }
        Ok(validate(&path)?.map(|value| Lease { value, lock }))
    }

    pub fn install<T>(
        &self,
        host: &impl Host,
        validate: impl FnOnce(&Path) -> Result<Option<T>, String>,
        build: impl FnOnce(&Path) -> Result<(PathBuf, T), String>,
    ) -> Result<Lease<T>, String> {
        let _install = self.install_lock(host)?;
        self.clear_staging(host)?;
        if let Some(lease) = self.get(validate)? {
            return Ok(lease);
        }
        let staging = Stage::new(&self.folder)?;
        let (source, value) = build(staging.path())?;
        let prunable = self.lock_prunable_versions(host)?;
        let destination = self.path();
        // A run may still be using Node without npm. Only replacement needs
        // to wait for those readers; staging and validation can proceed.
        let exclusive = self.use_file(self.pin)?;
        at(&destination, exclusive.lock())?;
        if at(&destination, absent(fs::symlink_metadata(&destination)))?.is_some() {
            let displaced = Stage::new(&self.folder)?;
            let old = displaced.path().join("displaced");
            at(&destination, fs::rename(&destination, &old))?;
            at(&old, remove_entry(&old))?;
        }
        at(&destination, fs::rename(source, &destination))?;
        drop(exclusive);
        let lock = self.shared()?;
        for (path, _use_lock) in prunable {
            if let Err(error) = host.remove_version(&path) {
                host.warn(&format!("could not prune {}: {error}", path.display()));
            }
        }
        Ok(Lease { value, lock })
    }

    fn install_lock(&self, host: &impl Host) -> Result<InstallLock, String> {
        let lock = lock_file(&self.folder, ".lock")?;
        let path = self.folder.path().join(".lock");
        match lock.try_lock() {
            Ok(()) => (),
            Err(TryLockError::WouldBlock) => {
                host.waiting();
                at(&path, lock.lock())?;
            }
            Err(TryLockError::Error(error)) => return at(&path, Err(error)),
        }
        Ok(InstallLock(lock))
    }

    fn entries(&self, host: &impl Host) -> Vec<fs::DirEntry> {
        let entries = match fs::read_dir(self.folder.path()) {
            Ok(entries) => entries,
            Err(error) => {
                host.warn(&format!(
                    "could not list {}: {error}",
                    self.folder.path().display()
                ));
                return Vec::new();
            }
        };
        entries
            .filter_map(|entry| match entry {
                Ok(entry) => Some(entry),
                Err(error) => {
                    host.warn(&format!(
                        "could not list an entry under {}: {error}",
                        self.folder.path().display()
                    ));
                    None
                }
            })
            .collect()
    }

    fn clear_staging(&self, host: &impl Host) -> Result<(), String> {
        for entry in self.entries(host) {
            let path = entry.path();
            let name = entry.file_name();
            if name.to_string_lossy().starts_with(".staging-") && existing_dir(&path) {
                at(&path, self.folder.verify_child_owner(&name))?;
                if let Err(error) = remove_entry(&path) {
                    host.warn(&format!(
                        "could not remove abandoned staging {}: {error}",
                        path.display()
                    ));
                }
            }
        }
        Ok(())
    }

    // Check all retention locks before publishing so a refused lock cannot
    // leave a partially pruned install. Contended versions are not touched.
    fn lock_prunable_versions(&self, host: &impl Host) -> Result<Vec<(PathBuf, File)>, String> {
        let pin = pins::version(self.pin).expect("tool pins are numeric versions");
        let older: Vec<_> = self
            .entries(host)
            .into_iter()
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                let number = pins::version(&name)?;
                (number < pin && existing_dir(&entry.path())).then_some((
                    number,
                    name,
                    entry.path(),
                ))
            })
            .collect();
        let keep = older.iter().map(|(number, _, _)| *number).max();
        let mut prunable = Vec::new();
        for (number, name, path) in older {
            if Some(number) == keep {
                continue;
            }
            let lock = self.use_file(&name)?;
            match lock.try_lock() {
                Ok(()) => prunable.push((path, lock)),
                Err(TryLockError::WouldBlock) => (),
                Err(TryLockError::Error(error)) => return at(&path, Err(error)),
            }
        }
        Ok(prunable)
    }
}

// Windows creates a new process directly, so it has no pre-exec fork interval
// in which a child could inherit this lock before close-on-exec takes effect.
#[cfg(all(test, unix))]
mod tests {
    use std::{
        io::{Read, Write},
        os::fd::AsRawFd,
        process::Command,
        thread,
    };

    use cortexkit_test_support::ScratchDir;
    use subc_os::fork_exec_test::{flags_are_close_on_exec, pause_before_exec};

    use super::{super::host::System, *};

    #[test]
    fn install_unlocks_while_a_concurrent_fork_is_still_before_exec() {
        let scratch = ScratchDir::new("rescue-install-lock");
        let folder = Directory::root(scratch.path(), "node", true)
            .unwrap()
            .unwrap();
        let store = Store::new(folder, pins::NODE_VERSION);
        let install = store.install_lock(&System).unwrap();
        let next = lock_file(&store.folder, ".lock").unwrap();
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 0"]);
        let (mut ready, mut release) =
            pause_before_exec(&mut command, install.0.as_raw_fd()).unwrap();
        let spawn = thread::spawn(move || command.status());

        let mut observation = [0; 8];
        let observed = ready.read_exact(&mut observation);
        let before_drop = next.try_lock();
        drop(install);
        let after_drop = next.try_lock();

        // Release and reap the paused child before asserting the lock results.
        let released = release.write_all(&[1]);
        let status = spawn.join().unwrap().unwrap();
        observed.unwrap();
        released.unwrap();
        assert!(status.success());
        let pid = i32::from_ne_bytes(observation[..4].try_into().unwrap());
        let flags = i32::from_ne_bytes(observation[4..].try_into().unwrap());
        assert!(flags_are_close_on_exec(flags));
        assert!(matches!(before_drop, Err(TryLockError::WouldBlock)));
        after_drop.unwrap_or_else(|error| {
            panic!("paused child {pid} retained the install lock: {error:?}")
        });
    }
}
