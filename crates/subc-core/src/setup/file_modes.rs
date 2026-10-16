use std::{fs, path::Path};

#[cfg(unix)]
use std::io;

use super::inventory::Inventory;

#[cfg(unix)]
use std::{
    ffi::OsString,
    fs::File,
    io::Write,
    os::{
        fd::OwnedFd,
        unix::fs::{DirBuilderExt, PermissionsExt},
    },
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(unix)]
use rustix::{
    fs::{fchmod, fstat, open as rustix_open, FileType, Mode, OFlags},
    io::Errno,
};

pub(super) fn ensure_owned_directory(path: &Path, mode_ceiling: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        match open_nofollow(path, true) {
            Ok(fd) => {
                let (file_type, current_mode) = descriptor_type_and_mode(&fd, path)?;
                if file_type != FileType::Directory {
                    return Err(format!(
                        "managed directory {} is not a directory",
                        path.display()
                    ));
                }
                set_descriptor_mode(&fd, path, current_mode & mode_ceiling)
            }
            Err(error) if error == Errno::NOENT => {
                create_missing_parent_directories(path)?;
                create_directory_exact(path, mode_ceiling)
            }
            Err(error) => Err(format!(
                "could not securely open managed directory {}: {error}",
                path.display()
            )),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = mode_ceiling;
        fs::create_dir_all(path).map_err(|error| {
            format!(
                "could not create managed directory {}: {error}",
                path.display()
            )
        })
    }
}

pub(super) fn tighten_owned_directory(path: &Path, mode_ceiling: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        let fd = match open_nofollow(path, true) {
            Ok(fd) => fd,
            Err(error) if is_missing_or_symlink(error) => return Ok(()),
            Err(error) if error == Errno::ACCESS => {
                let metadata = match fs::symlink_metadata(path) {
                    Ok(metadata) if metadata.file_type().is_dir() => metadata,
                    Ok(_) => return Ok(()),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                    Err(error) => {
                        return Err(format!(
                            "could not inspect managed directory {}: {error}",
                            path.display()
                        ));
                    }
                };
                let current_mode = metadata.permissions().mode() & 0o7777;
                if current_mode & mode_ceiling == current_mode {
                    return Ok(());
                }
                return Err(format!(
                    "cannot securely open loose-mode managed directory {}",
                    path.display()
                ));
            }
            Err(error) => {
                return Err(format!(
                    "could not securely open managed directory {}: {error}",
                    path.display()
                ));
            }
        };
        let (file_type, current_mode) = descriptor_type_and_mode(&fd, path)?;
        if file_type != FileType::Directory {
            return Ok(());
        }
        set_descriptor_mode(&fd, path, current_mode & mode_ceiling)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode_ceiling);
        Ok(())
    }
}

pub(super) fn directory_needs_tightening(path: &Path, mode_ceiling: u32) -> Result<bool, String> {
    #[cfg(unix)]
    {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                let mode = metadata.permissions().mode() & 0o7777;
                Ok(mode & mode_ceiling != mode)
            }
            Ok(_) => Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!(
                "could not inspect managed directory {}: {error}",
                path.display()
            )),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode_ceiling);
        Ok(false)
    }
}

/// Creates a fresh, private staging directory. Existing paths are never
/// reused: a collision could make a download share a directory with another
/// process or with files that setup did not create.
pub(super) fn create_private_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        create_missing_parent_directories(path)?;
        create_directory_exact(path, 0o700)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path).map_err(|error| {
            format!(
                "could not create staging directory {}: {error}",
                path.display()
            )
        })
    }
}

/// Creates only missing parent directories with owner-only access. Existing
/// parents belong to the user or the platform and are left unchanged.
pub(super) fn ensure_private_parents(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        create_missing_parents(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path).map_err(|error| {
            format!(
                "could not create parent directory {}: {error}",
                path.display()
            )
        })
    }
}

pub(super) fn set_new_file_mode(path: &Path, mode: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        let fd = open_nofollow(path, false).map_err(|error| {
            format!(
                "could not securely open new file {} to set its mode: {error}",
                path.display()
            )
        })?;
        let (file_type, _) = descriptor_type_and_mode(&fd, path)?;
        if file_type != FileType::RegularFile {
            return Err(format!("{} is not a regular file", path.display()));
        }
        set_descriptor_mode(&fd, path, mode)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

#[cfg(unix)]
pub(super) fn create_new_file(path: &Path, mode: u32) -> Result<(), String> {
    let raw_mode = mode_from_u32(mode, path)?;
    let fd = rustix_open(
        path,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        raw_mode,
    )
    .map_err(|error| format!("could not securely create {}: {error}", path.display()))?;
    fchmod(&fd, raw_mode)
        .map_err(|error| format!("could not set mode on {}: {error}", path.display()))
}

pub(super) fn copy_file_with_mode(
    source: &Path,
    destination: &Path,
    mode: u32,
) -> Result<(), String> {
    #[cfg(unix)]
    {
        let source_fd = rustix_open(
            source,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|error| format!("could not securely open {}: {error}", source.display()))?;
        let source_stat = fstat(&source_fd).map_err(|error| {
            format!(
                "could not inspect opened source {}: {error}",
                source.display()
            )
        })?;
        let source_type = FileType::from_raw_mode(source_stat.st_mode);
        if source_type != FileType::RegularFile {
            return Err(format!("{} is not a regular file", source.display()));
        }
        let mut source_file = File::from(source_fd);
        let mode = mode_from_u32(mode, destination)?;
        let destination_fd = rustix_open(
            destination,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|error| {
            format!(
                "could not securely create {}: {error}",
                destination.display()
            )
        })?;
        let mut destination_file = File::from(destination_fd);
        let result = (|| {
            std::io::copy(&mut source_file, &mut destination_file).map_err(|error| {
                format!(
                    "could not copy {} to {}: {error}",
                    source.display(),
                    destination.display()
                )
            })?;
            fchmod(&destination_file, mode).map_err(|error| {
                format!("could not set mode on {}: {error}", destination.display())
            })
        })();
        if result.is_err() {
            drop(destination_file);
            let _ = fs::remove_file(destination);
        }
        result
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        fs::copy(source, destination).map(|_| ()).map_err(|error| {
            format!(
                "could not copy {} to {}: {error}",
                source.display(),
                destination.display()
            )
        })
    }
}

pub(super) fn tighten_owned_file(path: &Path, mode_ceiling: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        tighten_owned_file_with_hook(path, mode_ceiling, || {})
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode_ceiling);
        Ok(())
    }
}

#[cfg(unix)]
fn tighten_owned_file_with_hook(
    path: &Path,
    mode_ceiling: u32,
    after_open: impl FnOnce(),
) -> Result<(), String> {
    let fd = match open_nofollow(path, false) {
        Ok(fd) => fd,
        Err(error) if is_missing_or_symlink(error) => return Ok(()),
        Err(error) if error == Errno::ACCESS => {
            let metadata = match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_file() => metadata,
                Ok(_) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => {
                    return Err(format!(
                        "could not inspect owned file {}: {error}",
                        path.display()
                    ));
                }
            };
            let current_mode = metadata.permissions().mode() & 0o7777;
            if current_mode & mode_ceiling == current_mode {
                return Ok(());
            }
            return Err(format!(
                "cannot securely open loose-mode owned file {}",
                path.display()
            ));
        }
        Err(error) => {
            return Err(format!(
                "could not securely open owned file {}: {error}",
                path.display()
            ));
        }
    };
    let (file_type, current_mode) = descriptor_type_and_mode(&fd, path)?;
    if file_type != FileType::RegularFile {
        return Ok(());
    }
    after_open();
    set_descriptor_mode(&fd, path, current_mode & mode_ceiling)
}

#[cfg(unix)]
fn open_nofollow(path: &Path, directory: bool) -> Result<OwnedFd, Errno> {
    let mut flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    if directory {
        flags |= OFlags::DIRECTORY;
    }
    rustix_open(path, flags, Mode::empty())
}

#[cfg(unix)]
fn descriptor_type_and_mode(fd: &OwnedFd, path: &Path) -> Result<(FileType, u32), String> {
    let stat = fstat(fd)
        .map_err(|error| format!("could not inspect opened path {}: {error}", path.display()))?;
    let file_type = FileType::from_raw_mode(stat.st_mode);
    #[cfg(target_os = "linux")]
    let mode = Mode::from_raw_mode(stat.st_mode).as_raw_mode() & 0o7777;
    #[cfg(not(target_os = "linux"))]
    let mode = u32::from(Mode::from_raw_mode(stat.st_mode).as_raw_mode() & 0o7777);
    Ok((file_type, mode))
}

#[cfg(unix)]
fn set_descriptor_mode(fd: &OwnedFd, path: &Path, mode: u32) -> Result<(), String> {
    let mode = mode_from_u32(mode, path)?;
    fchmod(fd, mode).map_err(|error| format!("could not set mode on {}: {error}", path.display()))
}

#[cfg(unix)]
fn is_missing_or_symlink(error: Errno) -> bool {
    error == Errno::NOENT || error == Errno::LOOP || error == Errno::NOTDIR
}

#[cfg(target_os = "linux")]
fn mode_from_u32(mode: u32, _path: &Path) -> Result<Mode, String> {
    Ok(Mode::from_raw_mode(mode))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn mode_from_u32(mode: u32, path: &Path) -> Result<Mode, String> {
    let raw_mode = u16::try_from(mode).map_err(|_| {
        format!(
            "mode for {} does not fit the platform's file mode type",
            path.display()
        )
    })?;
    Ok(Mode::from_raw_mode(raw_mode))
}

pub(super) fn file_needs_tightening(path: &Path, mode_ceiling: u32) -> Result<bool, String> {
    #[cfg(unix)]
    {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                let mode = metadata.permissions().mode() & 0o7777;
                Ok(mode & mode_ceiling != mode)
            }
            Ok(_) => Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!(
                "could not inspect owned file {}: {error}",
                path.display()
            )),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode_ceiling);
        Ok(false)
    }
}

/// Repairs only paths that setup itself installed, as listed in `installer-manifest.json`.
pub(super) fn tighten_inventory_files(inventory: &Inventory) -> Result<(), String> {
    for kind in ["managed-binary", "binary-placement"] {
        for path in inventory.paths_for_kind(kind) {
            tighten_owned_file(&path, 0o755)?;
        }
    }
    for path in inventory.paths_for_kind("configuration") {
        tighten_owned_file(&path, 0o600)?;
    }
    for kind in ["runtime-definition", "runtime-registration"] {
        for path in inventory.paths_for_kind(kind) {
            tighten_owned_file(&path, 0o644)?;
        }
    }
    Ok(())
}

/// Writes through a same-directory temporary and assigns its final mode before
/// rename. Existing restrictive mode bits survive an update; new files receive
/// the requested default mode.
pub(super) fn write_atomic(
    path: &Path,
    bytes: &[u8],
    default_mode: u32,
    mode_ceiling: u32,
    non_unix_temp_extension: &str,
) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("managed file {} has no parent directory", path.display()))?;
    ensure_private_parents(parent)?;

    #[cfg(unix)]
    {
        let _ = non_unix_temp_extension;
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let mode = match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                metadata.permissions().mode() & mode_ceiling
            }
            Ok(_) => default_mode,
            Err(error) if error.kind() == io::ErrorKind::NotFound => default_mode,
            Err(error) => {
                return Err(format!("could not inspect {}: {error}", path.display()));
            }
        };
        let temporary = unique_temporary_path(path, SEQUENCE.fetch_add(1, Ordering::Relaxed));
        let mut created = false;
        let result = (|| {
            let fd = rustix_open(
                &temporary,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::from_raw_mode(0o600),
            )
            .map_err(|error| {
                format!(
                    "could not create temporary file {}: {error}",
                    temporary.display()
                )
            })?;
            created = true;
            let mut file = File::from(fd);
            file.write_all(bytes).map_err(|error| {
                format!(
                    "could not write temporary file {}: {error}",
                    temporary.display()
                )
            })?;
            fchmod(&file, mode_from_u32(mode, path)?).map_err(|error| {
                format!(
                    "could not set mode on temporary file {}: {error}",
                    temporary.display()
                )
            })?;
            fs::rename(&temporary, path).map_err(|error| {
                format!(
                    "could not replace {} with {}: {error}",
                    path.display(),
                    temporary.display()
                )
            })
        })();
        if result.is_err() && created {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
    #[cfg(not(unix))]
    {
        let _ = (default_mode, mode_ceiling);
        let temporary = path.with_extension(non_unix_temp_extension);
        fs::write(&temporary, bytes)
            .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
        fs::rename(&temporary, path).map_err(|error| {
            format!(
                "could not replace {} with {}: {error}",
                path.display(),
                temporary.display()
            )
        })
    }
}

#[cfg(unix)]
fn create_missing_parents(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() || path.exists() {
        if path.exists() && !fs::metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
            return Err(format!("{} is not a directory", path.display()));
        }
        return Ok(());
    }
    let parent = path.parent().filter(|parent| *parent != path);
    if let Some(parent) = parent {
        create_missing_parents(parent)?;
    }
    match create_directory_exact(path, 0o700) {
        Ok(()) => Ok(()),
        Err(_) if fs::metadata(path).is_ok_and(|metadata| metadata.is_dir()) => Ok(()),
        Err(error) => Err(format!(
            "could not create private directory {}: {error}",
            path.display()
        )),
    }
}

#[cfg(unix)]
fn create_missing_parent_directories(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent().filter(|parent| *parent != path) {
        create_missing_parents(parent)?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_directory_exact(path: &Path, mode: u32) -> Result<(), String> {
    fs::DirBuilder::new()
        .mode(mode)
        .create(path)
        .map_err(|error| {
            format!(
                "could not create managed directory {}: {error}",
                path.display()
            )
        })?;
    set_directory_mode(path, mode)
}

#[cfg(unix)]
fn set_directory_mode(path: &Path, mode: u32) -> Result<(), String> {
    let fd = open_nofollow(path, true).map_err(|error| {
        format!(
            "could not securely open directory {} to set its mode: {error}",
            path.display()
        )
    })?;
    let (file_type, _) = descriptor_type_and_mode(&fd, path)?;
    if file_type != FileType::Directory {
        return Err(format!("{} is not a directory", path.display()));
    }
    set_descriptor_mode(&fd, path, mode)
}

#[cfg(unix)]
fn unique_temporary_path(path: &Path, sequence: u64) -> PathBuf {
    let mut name = OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(format!(".tmp-{}-{sequence}", std::process::id()));
    path.with_file_name(name)
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        os::unix::fs::{symlink, FileTypeExt, PermissionsExt},
        process::Command,
        sync::mpsc,
        time::Duration,
    };

    use cortexkit_test_support::ScratchDir;

    use super::*;

    #[test]
    fn tighten_owned_file_does_not_chmod_a_symlink_swapped_in_after_open() {
        let root = ScratchDir::new("mode-symlink-race");
        let install = root.join("install");
        let outside = root.join("outside");
        fs::create_dir_all(&install).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let owned = install.join("ck-aft");
        let outside_file = outside.join("shared-data");
        let displaced = install.join("opened-inode");
        fs::write(&owned, b"managed binary").unwrap();
        fs::write(&outside_file, b"external file").unwrap();
        fs::set_permissions(&owned, fs::Permissions::from_mode(0o666)).unwrap();
        fs::set_permissions(&outside_file, fs::Permissions::from_mode(0o666)).unwrap();

        tighten_owned_file_with_hook(&owned, 0o755, || {
            fs::rename(&owned, &displaced).unwrap();
            symlink(&outside_file, &owned).unwrap();
        })
        .unwrap();

        assert_eq!(
            mode(&outside_file),
            0o666,
            "the external target is untouched"
        );
        assert_eq!(
            mode(&displaced),
            0o644,
            "the opened managed inode is tightened"
        );
        assert!(fs::symlink_metadata(&owned)
            .unwrap()
            .file_type()
            .is_symlink());

        tighten_owned_file(&owned, 0o755).unwrap();
        assert!(set_new_file_mode(&owned, 0o755).is_err());
        assert_eq!(
            mode(&outside_file),
            0o666,
            "a symlink path is never chmodded"
        );

        let directory_link = install.join("data-link");
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o777)).unwrap();
        symlink(&outside, &directory_link).unwrap();
        tighten_owned_directory(&directory_link, 0o700).unwrap();
        assert!(ensure_owned_directory(&directory_link, 0o700).is_err());
        assert_eq!(mode(&outside), 0o777, "the external directory is untouched");
    }

    #[test]
    fn tighten_owned_file_returns_promptly_for_fifo_without_changing_its_mode() {
        let root = ScratchDir::new("mode-fifo-nonblocking");
        let fifo = root.join("managed-pipe");
        let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(status.success(), "mkfifo failed with {status}");
        assert!(
            fs::symlink_metadata(&fifo).unwrap().file_type().is_fifo(),
            "fixture path must be a FIFO"
        );
        fs::set_permissions(&fifo, fs::Permissions::from_mode(0o640)).unwrap();

        let worker_path = fifo.clone();
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done_tx.send(tighten_owned_file(&worker_path, 0o755));
        });
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("tightening a FIFO must not block while opening it")
            .unwrap();

        assert_eq!(mode(&fifo), 0o640, "the FIFO mode is unchanged");
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }
}
