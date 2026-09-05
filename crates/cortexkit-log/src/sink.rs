use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::Retention;

pub(crate) enum Destination {
    File(FileDestination),
    Fallback,
    #[cfg(test)]
    AlwaysFail,
}

impl Destination {
    pub(crate) fn open(
        path: &Path,
        retention: Retention,
        now: SystemTime,
        enforce_directory_mode: bool,
    ) -> io::Result<Self> {
        FileDestination::open(path, retention, now, enforce_directory_mode).map(Self::File)
    }

    pub(crate) fn write(&mut self, bytes: &[u8], now: SystemTime) -> io::Result<()> {
        match self {
            Self::File(file) => file.write(bytes, now),
            Self::Fallback => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "fallback writes use the diagnostic stream",
            )),
            #[cfg(test)]
            Self::AlwaysFail => Err(io::Error::other("injected write failure")),
        }
    }
}

pub(crate) struct FileDestination {
    path: PathBuf,
    file: Option<File>,
    retention: Retention,
}

impl FileDestination {
    fn open(
        path: &Path,
        retention: Retention,
        now: SystemTime,
        enforce_directory_mode: bool,
    ) -> io::Result<Self> {
        prepare_parent(path, enforce_directory_mode)?;
        prune_generations(path, retention, now)?;
        let file = open_active(path)?;
        Ok(Self {
            path: path.to_owned(),
            file: Some(file),
            retention,
        })
    }

    fn write(&mut self, bytes: &[u8], now: SystemTime) -> io::Result<()> {
        let current_len = self
            .file
            .as_ref()
            .ok_or_else(|| io::Error::other("log file is not open"))?
            .metadata()?
            .len();
        let cap = self.retention.max_bytes();
        if current_len > 0 && current_len.saturating_add(bytes.len() as u64) > cap {
            self.rotate(now)?;
        }
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("log file is not open"))?
            .write_all(bytes)
    }

    fn rotate(&mut self, now: SystemTime) -> io::Result<()> {
        drop(self.file.take());
        let rotation_result = rotate_paths(&self.path, self.retention, now);
        let reopen_result = open_active(&self.path);
        self.file = reopen_result.ok();

        rotation_result?;
        if self.file.is_none() {
            return Err(io::Error::other("rotated log file could not be reopened"));
        }
        Ok(())
    }
}

fn prepare_parent(path: &Path, enforce_directory_mode: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "log path has no parent"))?;
    let existed = parent.exists();
    fs::create_dir_all(parent)?;

    #[cfg(unix)]
    if enforce_directory_mode || !existed {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    // Windows has no mode bits to enforce; the per-user data directory's ACL is
    // inherited. The inputs exist only for the Unix arm above.
    #[cfg(not(unix))]
    let _ = (enforce_directory_mode, existed);

    Ok(())
}

fn open_active(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;

    #[cfg(unix)]
    if file.metadata()?.file_type().is_file() {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }

    Ok(file)
}

fn rotate_paths(path: &Path, retention: Retention, now: SystemTime) -> io::Result<()> {
    prune_generations(path, retention, now)?;

    if retention.keep == 0 {
        remove_if_present(path)?;
        return Ok(());
    }

    remove_if_present(&generation_path(path, u32::from(retention.keep)))?;
    for generation in (1..u32::from(retention.keep)).rev() {
        let source = generation_path(path, generation);
        if source.exists() {
            fs::rename(source, generation_path(path, generation + 1))?;
        }
    }
    if path.exists() {
        fs::rename(path, generation_path(path, 1))?;
    }

    prune_generations(path, retention, now)
}

fn prune_generations(path: &Path, retention: Retention, now: SystemTime) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "log path has no parent"))?;
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(());
    };
    if !parent.exists() {
        return Ok(());
    }

    let prefix = format!("{file_name}.");
    let max_age = Duration::from_secs(u64::from(retention.max_age_days) * 24 * 60 * 60);
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(suffix) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Ok(generation) = suffix.parse::<u32>() else {
            continue;
        };

        let metadata = entry.metadata()?;
        let too_many = generation > u32::from(retention.keep);
        let too_old = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > max_age);
        if too_many || too_old {
            remove_if_present(&entry.path())?;
        }
    }
    Ok(())
}

fn generation_path(path: &Path, generation: u32) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_owned();
    name.push(format!(".{generation}"));
    PathBuf::from(name)
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
pub(crate) fn rotated_path(path: &Path, generation: u32) -> PathBuf {
    generation_path(path, generation)
}
