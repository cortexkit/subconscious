use super::{capture, recovery_inventory::RecoveryArchive, GateError, Result};
use crate::config_store::ExpectedProjectFile;
use std::{
    ffi::{OsStr, OsString},
    fs::{File, Permissions},
    io::{Read, Seek, Write},
    path::Path,
};
use subc_config::InputState;

#[derive(Clone)]
pub(super) struct UserRestoreMetadata {
    directory: std::sync::Arc<File>,
    permissions: Option<Permissions>,
}

pub(super) struct ProjectRestore {
    pub target: crate::config_store::ProjectFileTarget,
    pub expected: ExpectedProjectFile,
    pub directory_id: Option<capture::FileObjectId>,
    pub permissions: Option<Permissions>,
    pub approved: Option<Vec<u8>>,
}

impl UserRestoreMetadata {
    pub(super) fn capture(file: &capture::CapturedFile) -> Option<Self> {
        let directory = subc_os::open_directory_nofollow_all(file.path.parent()?).ok()?;
        Some(Self {
            directory: std::sync::Arc::new(directory),
            permissions: file.permissions.clone(),
        })
    }

    pub(super) fn restore(
        self,
        path: &Path,
        expected: &InputState,
        bytes: Option<&[u8]>,
        archive: &RecoveryArchive,
        after_compare: impl FnOnce() -> Result<()>,
        after_last_read: impl FnOnce(),
    ) -> Result<()> {
        let current = subc_os::open_directory_nofollow_all(path.parent().ok_or_else(changed)?)
            .map_err(|_| changed())?;
        if capture::file_object_id(&current)? != capture::file_object_id(&self.directory)? {
            return Err(changed());
        }
        Restore {
            path,
            directory: &self.directory,
            expected: Expected::User(expected),
            approved: bytes,
            permissions: self.permissions,
            archive,
            layer: "user",
        }
        .run(after_compare, after_last_read)
    }
}

pub(super) enum Expected<'a> {
    User(&'a InputState),
    Project(&'a ExpectedProjectFile),
}

impl Expected<'_> {
    fn absent(&self) -> bool {
        matches!(
            self,
            Self::User(InputState::Absent) | Self::Project(ExpectedProjectFile::Absent)
        )
    }

    fn matches(&self, file: &mut File) -> Result<bool> {
        let before = file.metadata().map_err(write_error)?;
        file.rewind().map_err(write_error)?;
        let mut bytes = Vec::new();
        (&mut *file)
            .take(capture::CONFIG_FILE_LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(write_error)?;
        let after = file.metadata().map_err(write_error)?;
        if bytes.len() as u64 > capture::CONFIG_FILE_LIMIT
            || bytes.len() as u64 != before.len()
            || before.len() != after.len()
            || before.modified().ok() != after.modified().ok()
        {
            return Err(write_error("the comparison file was unstable or too large"));
        }
        Ok(match self {
            Self::User(expected) => {
                **expected == capture::opened_file_state(file, &before, &bytes)?
            }
            Self::Project(ExpectedProjectFile::Present { identity, digest }) => {
                *identity == capture::file_object_id(file)?
                    && *digest == subc_config::raw_digest(&bytes)
            }
            Self::Project(ExpectedProjectFile::Absent) => false,
        })
    }
}

pub(super) struct Restore<'a> {
    pub path: &'a Path,
    pub directory: &'a File,
    pub expected: Expected<'a>,
    pub approved: Option<&'a [u8]>,
    pub permissions: Option<Permissions>,
    pub archive: &'a RecoveryArchive,
    pub layer: &'static str,
}

impl<'a> Restore<'a> {
    pub(super) fn run(
        self,
        after_compare: impl FnOnce() -> Result<()>,
        after_last_read: impl FnOnce(),
    ) -> Result<()> {
        let leaf = self.path.file_name().ok_or_else(changed)?;
        // For an absent proposal, skip the move: conditional publication
        // refuses any file that arrives, and a no-op never opens it for write.
        if self.expected.absent() {
            if subc_os::open_single_link_file_nofollow_at(self.directory, leaf)
                .map_err(write_error)?
                .is_some()
            {
                return Err(changed());
            }
            after_compare()?;
            return self.publish(None).map_err(|(error, renamed)| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    changed()
                } else {
                    installed(write_error(error), renamed)
                }
            });
        }
        // Have an OS-resolved location to report even if a later lookup of the
        // retained source directory fails after the proposal has moved aside.
        let source_path = subc_os::retained_directory_path(self.directory).map_err(write_error)?;
        let inventory = self.archive.folder(self.directory)?;
        let _updates = inventory.serialize_updates()?;
        let moved = move_aside_for_restore(self.directory, leaf, |name, at| {
            inventory.record(self.path, &source_path.join(name), self.directory, at)
        })?;
        let put_back = |mut moved: subc_os::RetainedMovedFile<'a>, cause: GateError| match moved
            .relocate(self.directory, leaf)
        {
            Ok(()) => cause,
            Err(_) => {
                let newer = cause.code == "config_changed_since_review";
                let kept = self.keep(moved, leaf, newer, &source_path);
                kept.err()
                    .filter(|error| newer || error.wire_detail.is_some())
                    .unwrap_or(cause)
            }
        };
        let mut inspected = match moved.open().map_err(write_error) {
            Ok(file) => file,
            Err(error) => return Err(put_back(moved, error)),
        };
        match self.expected.matches(&mut inspected) {
            Ok(true) => {}
            comparison => return Err(put_back(moved, comparison.err().unwrap_or_else(changed))),
        }
        if let Err(error) = after_compare() {
            return Err(put_back(moved, error));
        }
        match self.expected.matches(&mut inspected) {
            Ok(true) => {}
            Ok(false) => return Err(put_back(moved, changed())),
            Err(error) => return Err(put_back(moved, error)),
        }
        after_last_read();
        if self.approved.is_none() {
            match subc_os::open_single_link_file_nofollow_at(self.directory, leaf) {
                Ok(Some(_)) => return self.keep(moved, leaf, true, &source_path),
                Ok(None) => {}
                Err(error) => return Err(put_back(moved, write_error(error))),
            }
        }
        if let Err((error, renamed)) = self.publish(Some(&inspected)) {
            if renamed || error.kind() == std::io::ErrorKind::AlreadyExists {
                return self
                    .keep(
                        moved,
                        leaf,
                        error.kind() == std::io::ErrorKind::AlreadyExists,
                        &source_path,
                    )
                    .and_then(|()| Err(write_error(error)))
                    .map_err(|error| installed(error, renamed));
            }
            return Err(put_back(moved, write_error(error)));
        }
        // The compared inode can still have an editor's writable descriptor.
        // Keep it even when its bytes matched; no restore path unlinks it.
        self.keep(moved, leaf, false, &source_path)
            .map_err(|error| installed(error, true))
    }

    fn publish(&self, original: Option<&File>) -> std::result::Result<(), (std::io::Error, bool)> {
        let Some(bytes) = self.approved else {
            return Ok(());
        };
        let mut temp = if original.is_none() {
            subc_os::RetainedTempFile::create_for_publish(self.directory)
        } else {
            subc_os::RetainedTempFile::create(self.directory)
        }
        .map_err(|error| (error, false))?;
        #[cfg(unix)]
        if self.permissions.is_none() {
            let names = existing_settings_names(
                self.directory,
                self.path.file_name().expect("validated leaf"),
            );
            temp.cap_settings_creation_mode(&names)
                .map_err(|error| (error, false))?;
        }
        temp.file_mut()
            .write_all(bytes)
            .map_err(|error| (error, false))?;
        temp.publish_restored(
            self.path.file_name().expect("validated leaf"),
            original,
            self.permissions.clone(),
        )
        .map_err(|error| {
            let renamed = error.renamed();
            (error.into(), renamed)
        })
    }

    fn keep(
        &self,
        moved: subc_os::RetainedMovedFile<'a>,
        leaf: &OsStr,
        report: bool,
        source_path: &Path,
    ) -> Result<()> {
        let inventory = self.archive.folder(self.directory)?;
        self.keep_with(
            moved,
            leaf,
            report,
            source_path,
            |moved, archive, name, at| {
                let destination = subc_os::retained_directory_path(archive)?.join(name);
                if subc_os::probe_retained_regular_file_at(archive, name)? {
                    return Err(std::io::ErrorKind::AlreadyExists.into());
                }
                inventory
                    .record(self.path, &destination, archive, at)
                    .map_err(std::io::Error::other)?;
                moved.relocate(archive, name)
            },
        )
    }

    fn keep_with(
        &self,
        mut moved: subc_os::RetainedMovedFile<'a>,
        leaf: &OsStr,
        report: bool,
        source_path: &Path,
        mut archive_copy: impl FnMut(
            &mut subc_os::RetainedMovedFile<'a>,
            &'a File,
            &OsStr,
            u64,
        ) -> std::io::Result<()>,
    ) -> Result<()> {
        let archive = &self.archive.directory;
        let now = crate::clock::wall_now_ms();
        let folder = self.archive.folder(self.directory)?;
        let set_name = |at| archive_leaf(at, folder.short_key(), self.layer, leaf);
        let mut directory = self.directory;
        let mut archive_error = None;
        for offset in 0..32 {
            let at = now.saturating_add(offset);
            match archive_copy(&mut moved, archive, &set_name(at), at) {
                Ok(()) => {
                    directory = archive;
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    tracing::warn!(%error, "could not move settings recovery copy into the data folder");
                    archive_error = Some(error);
                    break;
                }
            }
        }
        let archived = std::ptr::eq(directory, archive);
        if !report && archived {
            return Ok(());
        }
        let name = subc_os::retained_directory_path(directory)
            .or_else(|error| {
                if archived {
                    Err(error)
                } else {
                    Ok(source_path.to_owned())
                }
            })
            .map_err(|_| {
                GateError::new(
                    "write_back_failed",
                    format!(
                "the settings recovery copy is named {}; the OS could not resolve its directory",
                moved.name().to_string_lossy()
            ),
                )
                .with_wire_detail(
                    serde_json::json!({"recovery_name": moved.name().to_string_lossy()}),
                )
            })?
            .join(moved.name());
        if !archived {
            tracing::warn!(recovery_path = %name.display(), "settings recovery copy kept beside the settings file");
        }
        let (code, message) = if report {
            (
                "config_changed_since_review",
                "the settings file changed; a recovery copy was kept".into(),
            )
        } else {
            (
                "write_back_failed",
                format!(
                    "the settings recovery copy could not be moved into the archive: {}",
                    archive_error.map_or_else(
                        || "recovery names collided".into(),
                        |error| error.to_string()
                    )
                ),
            )
        };
        Err(GateError::new(code, message)
            .with_wire_detail(serde_json::json!({"path":self.path, "recovery_path":name})))
    }
}

pub(super) fn archive_leaf(at: u64, folder_key: &str, layer: &str, leaf: &OsStr) -> OsString {
    let mut name = OsString::from(format!("{}-{folder_key}-{layer}-", recovery_stamp(at)));
    name.push(leaf);
    name
}

fn move_aside_for_restore<'a>(
    directory: &'a File,
    leaf: &OsStr,
    mut record: impl FnMut(&OsStr, u64) -> Result<()>,
) -> Result<subc_os::RetainedMovedFile<'a>> {
    let now = crate::clock::wall_now_ms();
    for offset in 0..32 {
        // Even if the filesystem refuses every subsequent rename, the bytes
        // are already at a recognizable recovery name, not a temporary leaf.
        let mut hidden = OsString::from(".");
        hidden.push(leaf);
        let at = now.saturating_add(offset);
        hidden.push(format!(".declined-{}", recovery_stamp(at)));
        record(&hidden, at)?;
        match subc_os::move_file_to_noreplace_at(directory, leaf, &hidden) {
            Ok(Some(moved)) => return Ok(moved),
            Ok(None) => return Err(changed()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(write_error(error)),
        }
    }
    Err(write_error("settings recovery names collided"))
}

#[cfg(unix)]
fn existing_settings_names(directory: &File, restore_leaf: &OsStr) -> Vec<OsString> {
    let Ok(path) = subc_os::retained_directory_path(directory) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut names = entries
        .flatten()
        .map(|entry| entry.file_name())
        .filter(|leaf| {
            leaf.as_os_str() != restore_leaf
                && Path::new(leaf).extension() == Some(OsStr::new("jsonc"))
        })
        .collect::<Vec<_>>();
    names.sort_unstable();
    names
}

fn recovery_stamp(at: u64) -> String {
    // A sortable UTC stamp without the colon prohibited in Windows leaves.
    crate::stderr_tail::format_capture_stamp(at).replace(':', "")
}

pub(super) fn changed() -> GateError {
    GateError::new(
        "config_changed_since_review",
        "the settings file changed since planning",
    )
}

fn write_error(_: impl std::fmt::Display) -> GateError {
    GateError::new(
        "write_back_failed",
        "could not safely restore the approved settings file",
    )
}

fn installed(mut error: GateError, renamed: bool) -> GateError {
    error.restore_installed = renamed;
    error
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn config_pending_settings_archive_move_failure_names_the_kept_copy() {
        let scratch = cortexkit_test_support::ScratchDir::new("settings-recovery-rename-refused");
        let data = scratch.canonicalize().unwrap();
        let path = data.join("a.jsonc");
        std::fs::write(&path, b"reviewed proposal").unwrap();
        let directory = subc_os::open_directory_nofollow_all(&data).unwrap();
        let archive = RecoveryArchive::open(
            &data,
            Default::default(),
            subc_os::probe_retained_directory_write,
        )
        .unwrap();
        let leaf = path.file_name().unwrap();
        let moved = move_aside_for_restore(&directory, leaf, |_, _| Ok(())).unwrap();
        std::fs::write(&path, b"approved contents").unwrap();
        let restore = Restore {
            path: &path,
            directory: &directory,
            expected: Expected::User(&InputState::Absent),
            approved: None,
            permissions: None,
            archive: &archive,
            layer: "user",
        };
        let error = restore
            .keep_with(moved, leaf, false, &data, |_, _, _, _| {
                Err(std::io::ErrorKind::PermissionDenied.into())
            })
            .unwrap_err();
        assert_eq!(error.code, "write_back_failed");
        let detail = error.wire_detail.unwrap();
        let recovery = Path::new(detail["recovery_path"].as_str().unwrap());
        assert!(recovery
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".a.jsonc.declined-"));
        assert_eq!(std::fs::read(recovery).unwrap(), b"reviewed proposal");
        assert_eq!(std::fs::read(&path).unwrap(), b"approved contents");
    }

    #[test]
    fn config_pending_settings_cross_device_copy_reports_the_retained_directory_path() {
        let scratch = cortexkit_test_support::ScratchDir::new("cross-device-settings-recovery");
        let data = scratch.canonicalize().unwrap();
        let settings = data.join("settings");
        let relocated = data.join("renamed");
        std::fs::create_dir(&settings).unwrap();
        let path = settings.join("demo.jsonc");
        std::fs::write(&path, b"moved settings").unwrap();
        let directory = subc_os::open_directory_nofollow_all(&settings).unwrap();
        let archive = RecoveryArchive::open(
            &data,
            Default::default(),
            subc_os::probe_retained_directory_write,
        )
        .unwrap();
        let leaf = path.file_name().unwrap();
        let moved = move_aside_for_restore(&directory, leaf, |_, _| Ok(())).unwrap();
        std::fs::rename(&settings, &relocated).unwrap();
        let restore = Restore {
            path: &path,
            directory: &directory,
            expected: Expected::User(&InputState::Absent),
            approved: None,
            permissions: None,
            archive: &archive,
            layer: "user",
        };
        let error = restore
            .keep_with(moved, leaf, true, &settings, |_, _, _, _| {
                Err(std::io::Error::from(std::io::ErrorKind::CrossesDevices))
            })
            .unwrap_err();
        let recovery = error.wire_detail.unwrap()["recovery_path"]
            .as_str()
            .unwrap()
            .to_owned();
        let recovery = Path::new(&recovery);
        assert_eq!(recovery.parent(), Some(relocated.as_path()));
        assert!(recovery
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with(".demo.jsonc.declined-"));
        assert_eq!(std::fs::read(recovery).unwrap(), b"moved settings");
    }
}
