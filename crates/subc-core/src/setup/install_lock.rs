//! One installer command at a time per install root.
//!
//! Setup, upgrade, uninstall and the verified bootstrap helper all recover or
//! update the same journal and inventory. The advisory lock on `installer.lock`
//! (`flock` on Unix, `LockFileEx` on Windows) covers recovery, ownership checks,
//! placement and commit. It is released while waiting for confirmation, then
//! reacquired before revalidating the plan. The OS releases it on process exit.

use std::{
    fs::{self, File, OpenOptions, TryLockError},
    path::{Path, PathBuf},
    process,
    sync::{Arc, Mutex, Weak},
    thread,
    time::{Duration, Instant},
};

/// How long a second command waits for the first to finish.
pub(super) const WAIT: Duration = Duration::from_secs(30);

const LOCK_NAME: &str = "installer.lock";
/// Names the command holding the lock. A separate file, because on Windows
/// the locked file cannot be read by anyone else while it is held.
const HOLDER_NAME: &str = "installer.lock.holder";

/// Held for as long as an installer command may change the install root.
#[derive(Debug)]
pub struct InstallLock {
    _held: Arc<Held>,
}

/// The locked file and the holder note naming this command. Shared between
/// the [`InstallHold`] an applying command takes first and the inventories
/// it loads afterwards, and released when the last of them is dropped.
#[derive(Debug)]
struct Held {
    _file: File,
    lock: PathBuf,
    holder: PathBuf,
}

/// The lock an applying command (`ck upgrade`, the app's upgrade) takes
/// before it reads anything it will act on: discovery, the roster, the
/// ownership check, planning and every inventory it then loads run under
/// this one hold. Later acquisitions of the same lock in this process share
/// it instead of waiting on it.
#[derive(Debug)]
pub struct InstallHold {
    _lock: InstallLock,
}

/// The one hold this process has taken, which later acquisitions of the same
/// lock share.
static PROCESS_HOLD: Mutex<Option<Weak<Held>>> = Mutex::new(None);

impl InstallHold {
    /// Takes the install lock for the root holding `manifest`, waiting up
    /// to `wait`, and keeps it until the hold is dropped.
    pub(super) fn take(manifest: &Path, command: &str, wait: Duration) -> Result<Self, String> {
        let lock = InstallLock::acquire_within(manifest, command, wait)?;
        *PROCESS_HOLD
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(Arc::downgrade(&lock._held));
        Ok(Self { _lock: lock })
    }

    /// Whether this process holds the lock for the root holding `manifest`
    /// through an [`InstallHold`].
    pub(super) fn is_held(manifest: &Path) -> bool {
        let Some(root) = manifest.parent() else {
            return false;
        };
        PROCESS_HOLD
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|held| held.lock == root.join(LOCK_NAME))
    }
}

impl InstallLock {
    /// Locks the install root holding `manifest`, waiting up to `wait` for
    /// another installer command to finish. `command` names this one in the
    /// refusal another command would see.
    pub(super) fn acquire_within(
        manifest: &Path,
        command: &str,
        wait: Duration,
    ) -> Result<InstallLock, String> {
        let root = manifest.parent().ok_or_else(|| {
            format!(
                "installer manifest {} has no parent directory",
                manifest.display()
            )
        })?;
        let path = root.join(LOCK_NAME);
        if let Some(held) = PROCESS_HOLD
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .and_then(Weak::upgrade)
            .filter(|held| held.lock == path)
        {
            return Ok(InstallLock { _held: held });
        }
        super::file_modes::ensure_owned_directory(root, 0o700)?;
        let file = open_lock_file(&path)?;
        let deadline = Instant::now() + wait;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(TryLockError::WouldBlock) => {
                    let holder = read_regular(&root.join(HOLDER_NAME))
                        .map(|text| text.trim().to_string())
                        .filter(|text| !text.is_empty())
                        .map_or_else(
                            || "another installer command".to_string(),
                            |text| format!("another installer command ({text})"),
                        );
                    return Err(format!(
                        "refusal: {holder} is changing the install at {}; it still held {} after {} s. Wait for it to finish and run the command again",
                        root.display(),
                        path.display(),
                        wait.as_secs()
                    ));
                }
                Err(TryLockError::Error(error)) => {
                    return Err(format!("could not lock {}: {error}", path.display()))
                }
            }
        }
        let holder = root.join(HOLDER_NAME);
        // Only a courtesy for the refusal text: a failure here does not
        // weaken the lock itself.
        let _ = write_holder(&holder, &format!("pid {}, {command}\n", process::id()));
        Ok(InstallLock {
            _held: Arc::new(Held {
                _file: file,
                lock: path,
                holder,
            }),
        })
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        // Removed while still holding the lock, so it never names a command
        // that has finished. Only a regular file is removed: anything else
        // there is not ours.
        if fs::symlink_metadata(&self.holder).is_ok_and(|metadata| metadata.is_file()) {
            let _ = fs::remove_file(&self.holder);
        }
        // Unlock explicitly instead of waiting for the last descriptor to
        // close: a concurrent fork can inherit it until the child execs.
        #[cfg(unix)]
        let _ = self._file.unlock();
    }
}

/// Writes the holder note to a new file with a name no one can have
/// prepared (pid and time), created exclusively and without following a
/// link, then renames it over the holder path. The rename replaces whatever
/// entry is there, a link included, rather than writing through it.
fn write_holder(holder: &Path, text: &str) -> Result<(), String> {
    #[cfg(unix)]
    {
        super::file_modes::write_atomic(holder, text.as_bytes(), 0o600, 0o600, "holder.tmp")
    }
    #[cfg(not(unix))]
    {
        use std::io::Write as _;
        let parent = holder.parent().ok_or("holder has no parent")?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let temporary = parent.join(format!(".{HOLDER_NAME}.{}.{nonce}", process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let written = (|| {
            let mut file = options.open(&temporary)?;
            if is_reparse_point(&file.metadata()?) {
                return Err(std::io::Error::other("the new holder file is a link"));
            }
            file.write_all(text.as_bytes())?;
            drop(file);
            fs::rename(&temporary, holder)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        written.map_err(|error| format!("could not write {}: {error}", holder.display()))
    }
}

/// `FILE_FLAG_OPEN_REPARSE_POINT`: open a link itself, never its target.
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

/// Whether an opened file is a link (a reparse point) rather than a file.
#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

/// Opens (creating if needed) the lock file, refusing anything but a regular
/// file this account owns: a link there could point the lock, and with it
/// the installer, at a file that is not the installer's.
fn open_lock_file(path: &Path) -> Result<File, String> {
    let refuse = |what: &str| {
        format!(
            "refusal: install lock {} is {what}; remove it and run the command again",
            path.display()
        )
    };
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => return Err(refuse("a link")),
        Ok(metadata) if !metadata.is_file() => return Err(refuse("not a regular file")),
        _ => {}
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // Refuses a link that appears between the check above and the open.
        options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        // Opens a link that appears after the check above as itself, which
        // the reparse-point check below then refuses.
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
    if is_reparse_point(&metadata) {
        return Err(refuse("a link"));
    }
    if !metadata.is_file() {
        return Err(refuse("not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(refuse("owned by another account"));
        }
    }
    Ok(file)
}

/// The text of `path` when it is a regular file, never following a link.
fn read_regular(path: &Path) -> Option<String> {
    fs::symlink_metadata(path)
        .ok()
        .filter(|metadata| metadata.is_file())
        .and_then(|_| fs::read_to_string(path).ok())
}

#[cfg(test)]
mod tests;
