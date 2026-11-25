//! Atomic publication of a single leaf relative to a retained directory.

use crate::{retained_file_identity, DurableReplaceError, RetainedFileIdentity};
use std::{
    ffi::{OsStr, OsString},
    fs::{File, Permissions},
    io,
    path::{Component, Path, PathBuf},
};

#[cfg_attr(unix, path = "retained_replace/unix.rs")]
#[cfg_attr(windows, path = "retained_replace/windows.rs")]
mod platform;

/// An exclusive, no-follow staging file anchored to a retained directory.
/// Dropping it before publication attempts to remove the staging entry.
/// Windows flushes the renamed object opened in write-through mode; it does
/// not provide a separate retained-directory namespace flush. A failed flush
/// after publication is reported by [`DurableReplaceError::renamed`].
#[derive(Debug)]
pub struct RetainedTempFile<'a> {
    stage: Stage<'a>,
    for_publish: bool,
}

/// A file moved to a unique recovery name in a retained directory. Dropping
/// this value never deletes the recovery entry.
#[derive(Debug)]
pub struct RetainedMovedFile<'a> {
    parent: &'a File,
    name: OsString,
    #[cfg(windows)]
    file: File,
}

/// Refusals from a single-component read beneath a retained directory handle.
#[derive(Debug)]
pub enum OpenRetainedError {
    Io(io::Error),
    Symlink,
    NotRegular,
    Hardlinked,
}

impl From<io::Error> for OpenRetainedError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl std::fmt::Display for OpenRetainedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::Symlink => f.write_str("entry is a symlink or reparse point"),
            Self::NotRegular => f.write_str("entry does not have the requested file type"),
            Self::Hardlinked => f.write_str("entry is not a singly linked file"),
        }
    }
}

impl std::error::Error for OpenRetainedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

/// Open one child directory without following a symlink or reparse point.
/// `Ok(None)` means the child was absent when opened.
pub fn open_directory_nofollow_at(
    parent: &File,
    child: &OsStr,
) -> Result<Option<File>, OpenRetainedError> {
    open_read_at(parent, child, true)
}

/// Open one singly linked regular file without following a symlink or reparse
/// point. `Ok(None)` means the child was absent when opened.
pub fn open_single_link_file_nofollow_at(
    parent: &File,
    child: &OsStr,
) -> Result<Option<File>, OpenRetainedError> {
    open_read_at(parent, child, false)
}

/// Check a retained directory's regular leaf using metadata only. A link,
/// non-file, or unavailable metadata is not counted as a regular file. The
/// file need not grant permission to read its contents.
pub fn retained_regular_file_at(parent: &File, leaf: &OsStr) -> bool {
    probe_retained_regular_file_at(parent, leaf).unwrap_or(false)
}

/// Check a regular leaf by metadata without following links. `Ok(false)`
/// means the kernel reported absence; a nonregular leaf or inaccessible
/// location returns an error and must not be treated as proof of absence.
pub fn probe_retained_regular_file_at(parent: &File, leaf: &OsStr) -> io::Result<bool> {
    validate_leaf(leaf)?;
    platform::ensure_directory(parent)?;
    match platform::regular_file_at(parent, leaf) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn open_read_at(
    parent: &File,
    child: &OsStr,
    directory: bool,
) -> Result<Option<File>, OpenRetainedError> {
    validate_leaf(child)?;
    match platform::open_read_at(parent, child, directory) {
        Ok(file) => Ok(Some(file)),
        Err(OpenRetainedError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

impl<'a> RetainedTempFile<'a> {
    /// Stage a replacement. On Unix the staging mode is at most `0600`
    /// until the existing target's metadata is applied. On Windows the staging
    /// handle denies sharing until publication completes.
    pub fn create(parent: &'a File) -> io::Result<Self> {
        Self::create_impl(parent, false)
    }

    /// Stage a new file under the parent's ordinary creation permissions:
    /// on Unix, mode `0666` subject to the default ACL and/or umask; on
    /// Windows, the inherited security descriptor.
    pub fn create_for_publish(parent: &'a File) -> io::Result<Self> {
        Self::create_impl(parent, true)
    }

    fn create_impl(parent: &'a File, for_publish: bool) -> io::Result<Self> {
        let stage = Stage::create(parent, false, for_publish)?;
        // Unlike a POSIX ACL mask, Darwin mode bits do not constrain an
        // inherited allow entry. Remove that ACL before writing any bytes.
        #[cfg(target_os = "macos")]
        if !for_publish {
            platform::remove_inherited_acl(&stage.file)?;
        }
        Ok(Self { stage, for_publish })
    }

    /// The open staging file to write. Publication flushes its data and metadata.
    pub fn file_mut(&mut self) -> &mut File {
        &mut self.stage.file
    }

    /// Limit the mode using the first regular sibling's metadata, or 0644.
    /// Unix stats names relative to the retained parent without following
    /// links or reading file contents. This cannot add bits removed at
    /// creation; macOS keeps the directory's inherited access-control list
    /// (ACL). Windows keeps its inherited security descriptor and does not
    /// inspect siblings.
    pub fn cap_settings_creation_mode(&mut self, candidates: &[OsString]) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let limit = candidates
                .iter()
                .find_map(|name| {
                    validate_leaf(name).ok()?;
                    platform::regular_mode_at(self.stage.parent, name)
                })
                .unwrap_or(0o644);
            let mode = self.stage.file.metadata()?.permissions().mode();
            self.stage
                .file
                .set_permissions(Permissions::from_mode(mode & limit & 0o777))?;
        }
        #[cfg(windows)]
        let _ = candidates;
        Ok(())
    }

    /// Replace an existing regular, singly linked `leaf`. It must still be
    /// the file identified by `expected`. The caller is responsible for checking
    /// expected contents. A concurrent writer can change the entry between the
    /// final identity check and the rename, which never follows a target link.
    ///
    /// On Unix, owner, group, mode and ACL are preserved or replacement fails.
    /// On Windows, the owner, group, discretionary ACL and integrity label are
    /// preserved or replacement fails. Audit rules (which record access in
    /// the security log) are not copied from the old file. The staging file
    /// inherits those rules from the parent without requiring audit privileges.
    pub fn replace(self, leaf: &OsStr, expected: &File) -> Result<(), DurableReplaceError> {
        self.replace_checked(leaf, expected, || Ok(()))
    }

    /// Like [`Self::replace`], but check the target after flushing staging and
    /// immediately before each rename attempt. A failed check is not retried
    /// and leaves the target unpublished.
    pub fn replace_checked(
        mut self,
        leaf: &OsStr,
        expected: &File,
        mut check: impl FnMut() -> io::Result<()>,
    ) -> Result<(), DurableReplaceError> {
        if self.for_publish {
            return Err(before(invalid("replacement requires private staging")));
        }
        validate_leaf(leaf).map_err(before)?;
        self.stage.verify().map_err(before)?;
        let expected = retained_file_identity(expected).map_err(before)?;
        let open_expected = || {
            let current = platform::open_target(self.stage.parent, leaf)?;
            if retained_file_identity(&current)? != expected {
                return Err(io::Error::other("replacement target identity changed"));
            }
            Ok(current)
        };
        let current = platform::retry(open_expected).map_err(before)?;
        platform::preserve_metadata(&current, &self.stage.file).map_err(|error| {
            before(io::Error::new(
                error.kind(),
                format!("cannot safely preserve replacement permissions: {error}"),
            ))
        })?;
        drop(current);
        platform::flush_staging(&self.stage.file, false).map_err(before)?;
        platform::retry(|| -> io::Result<io::Result<()>> {
            self.stage.verify()?;
            open_expected()?;
            if let Err(error) = check() {
                return Ok(Err(error));
            }
            platform::rename(&self.stage, leaf, true)?;
            Ok(Ok(()))
        })
        .and_then(|checked| checked)
        .map_err(before)?;
        self.stage.published = true;
        self.stage.flush_publication()
    }

    /// Publish at an absent `leaf` using Linux `renameat2(RENAME_NOREPLACE)`,
    /// macOS `renameatx_np(RENAME_EXCL)` or Windows handle-relative
    /// `NtSetInformationFile` without its replace flag. There is no fallback
    /// when the OS or filesystem cannot guarantee no replacement; this API is
    /// not provided on other operating systems.
    pub fn publish(self, leaf: &OsStr) -> Result<(), DurableReplaceError> {
        self.publish_impl(leaf, || Ok(()))
    }

    /// Publish a restored file only at an absent name. Keep the security
    /// metadata from `original` when supplied, with the recorded permission
    /// bits overriding its current bits. Without an original, keep the stage's
    /// private or inherited creation policy. A metadata failure or lack of a
    /// native no-replace rename refuses publication; an existing leaf is never
    /// replaced. Windows keeps the source's owner, group, discretionary
    /// access-control list (ACL) and integrity label, but does not copy audit
    /// rules; the staging file inherits those from the directory without
    /// requiring audit privileges.
    /// `renamed()` distinguishes a later flush failure.
    pub fn publish_restored(
        mut self,
        leaf: &OsStr,
        original: Option<&File>,
        permissions: Option<Permissions>,
    ) -> Result<(), DurableReplaceError> {
        if let Some(original) = original {
            platform::preserve_metadata(original, &self.stage.file).map_err(before)?;
        }
        if let Some(permissions) = permissions {
            self.stage
                .file
                .set_permissions(permissions)
                .map_err(before)?;
        }
        self.stage.publish(leaf)?;
        self.stage.flush_publication()
    }

    fn publish_impl(
        mut self,
        leaf: &OsStr,
        after_rename: impl FnOnce() -> io::Result<()>,
    ) -> Result<(), DurableReplaceError> {
        if !self.for_publish {
            return Err(before(invalid(
                "new file requires inherited-permission staging",
            )));
        }
        self.stage.publish(leaf)?;
        after_rename().map_err(after)?;
        self.stage.flush_publication()
    }
}

/// Move a leaf to an unused name in `parent`, without following a link or
/// replacing a destination. Unix checks the leaf type, then renames by name:
/// if it was swapped after that check, the moved entry can be a different
/// object. Windows moves the validated open object and denies write and delete
/// sharing while retaining it. `Ok(None)` means the source disappeared. An
/// unsupported native no-replace rename fails without a weaker fallback.
pub fn move_file_aside_nofollow_at<'a>(
    parent: &'a File,
    leaf: &OsStr,
) -> io::Result<Option<RetainedMovedFile<'a>>> {
    for _ in 0..8 {
        let name = random_name(".subc-recovery-")?;
        match move_file_to_noreplace_at(parent, leaf, name.as_ref()) {
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            result => return result,
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "recovery names collided",
    ))
}

/// Move a file to a caller-chosen absent leaf in the same retained directory.
/// Unix renames the validated name, which can be swapped before the rename;
/// Windows moves the validated open object while denying writes and deletes.
/// Links are not followed. Missing source returns `Ok(None)`; a collision
/// or unsupported no-replace operation returns the OS error without replacing
/// a destination or falling back to a weaker move.
pub fn move_file_to_noreplace_at<'a>(
    parent: &'a File,
    leaf: &OsStr,
    name: &OsStr,
) -> io::Result<Option<RetainedMovedFile<'a>>> {
    validate_leaf(leaf)?;
    validate_leaf(name)?;
    platform::ensure_directory(parent)?;
    match platform::move_aside(parent, leaf, name) {
        Ok(_file) => Ok(Some(RetainedMovedFile {
            parent,
            name: name.to_owned(),
            #[cfg(windows)]
            file: _file,
        })),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

impl RetainedMovedFile<'_> {
    /// The single-component recovery name relative to the retained parent.
    pub fn name(&self) -> &OsStr {
        self.name.as_ref()
    }

    /// Open the moved entry for inspection without following links. A missing,
    /// unreadable or nonregular entry returns an error and is not removed.
    pub fn open(&self) -> Result<File, OpenRetainedError> {
        #[cfg(unix)]
        return platform::open_read_at(self.parent, self.name(), false);
        #[cfg(windows)]
        return self.file.try_clone().map_err(OpenRetainedError::Io);
    }

    /// Put the recovery entry back at an absent leaf without replacing it.
    /// On Unix this moves the entry at the retained name, not an inspected
    /// object; Windows renames the retained object. If no-replace rename is
    /// unsupported or the destination exists, return the OS error and keep
    /// the recovery entry.
    pub fn put_back(self, leaf: &OsStr) -> io::Result<()> {
        validate_leaf(leaf)?;
        platform::put_back(&self, leaf)
    }

    /// Delete the recovery entry, not the original leaf. Windows deletes the
    /// retained object; Unix compares identity then unlinks by name, and an
    /// entry swap in that interval is not prevented. This is for cleanup, not
    /// conditional deletion of live settings. A detectable mismatch or an
    /// unsupported operation returns an error without a weaker fallback.
    pub fn discard(self, inspected: &File) -> io::Result<()> {
        platform::discard_moved(&self, retained_file_identity(inspected)?)
    }
}

impl<'a> RetainedMovedFile<'a> {
    /// Move this entry to an absent name in a retained directory. Unix moves
    /// the current source name; Windows moves the open object. An existing
    /// destination, a cross-device move or lack of native no-replace support
    /// returns the OS error, without a copy/delete fallback.
    pub fn relocate(&mut self, destination: &'a File, leaf: &OsStr) -> io::Result<()> {
        validate_leaf(leaf)?;
        platform::ensure_directory(destination)?;
        platform::relocate(self, destination, leaf)?;
        self.parent = destination;
        self.name = leaf.to_owned();
        Ok(())
    }
}

/// Return the OS's current path for an open directory, for reporting only.
/// An unsupported lookup returns an error; no configured path is substituted.
pub fn retained_directory_path(directory: &File) -> io::Result<PathBuf> {
    platform::ensure_directory(directory)?;
    platform::directory_path(directory)
}

/// Open or create a single owner-only directory beneath a retained parent.
/// Unix uses mode 0700; Windows uses a protected discretionary access control
/// list (DACL) that grants only the owner. An existing
/// directory must already be private; an unsupported creation or verification
/// returns an error without adopting a less restrictive directory.
pub fn open_or_create_private_directory_at(parent: &File, leaf: &OsStr) -> io::Result<File> {
    let open = || open_directory_nofollow_at(parent, leaf).map_err(io::Error::other);
    if let Some(dir) = open()? {
        platform::verify_private_directory(&dir)?;
        return Ok(dir);
    }
    let mut stage = Stage::create(parent, true, false)?;
    #[cfg(target_os = "macos")]
    platform::remove_inherited_acl(&stage.file)?;
    platform::verify_private_directory(&stage.file)?;
    if let Err(error) = stage.publish(leaf) {
        let error = io::Error::from(error);
        if error.kind() == io::ErrorKind::AlreadyExists {
            let dir = open()?.ok_or(error)?;
            platform::verify_private_directory(&dir)?;
            return Ok(dir);
        }
        return Err(error);
    }
    stage.flush_publication().map_err(io::Error::from)?;
    let identity = stage.identity;
    // The Windows staging handle denies delete sharing. Release it before a
    // cross-directory rename asks Windows to open the destination directory.
    drop(stage);
    let directory = open()?.ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
    if retained_file_identity(&directory)? != identity {
        return Err(invalid("published recovery directory was replaced"));
    }
    platform::verify_private_directory(&directory)?;
    Ok(directory)
}

/// Exclusively create and retain a directory beneath `parent`. It uses the
/// parent's default directory permissions (Unix mode `0777` filtered by the
/// default ACL and/or umask; inherited security on Windows). The same native
/// no-replace APIs as [`RetainedTempFile::publish`] are used; an existing leaf
/// is never opened or adopted.
pub fn create_directory_nofollow_at(
    parent: &File,
    leaf: &OsStr,
) -> Result<File, DurableReplaceError> {
    validate_leaf(leaf).map_err(before)?;
    let mut stage = Stage::create(parent, true, true).map_err(before)?;
    stage.publish(leaf)?;
    stage.flush_publication()?;
    // Stage owns cleanup until the rename succeeds; the returned handle is
    // an alias of that exact directory, not an open of its published name.
    stage.file.try_clone().map_err(after)
}

#[derive(Debug)]
struct Stage<'a> {
    parent: &'a File,
    #[cfg(unix)]
    name: String,
    file: File,
    identity: RetainedFileIdentity,
    directory: bool,
    published: bool,
}

impl<'a> Stage<'a> {
    fn create(parent: &'a File, directory: bool, for_publish: bool) -> io::Result<Self> {
        platform::ensure_directory(parent)?;
        for _ in 0..8 {
            let name = random_name(".subc-stage-")?;
            match platform::create(parent, &name, directory, for_publish) {
                Ok(file) => {
                    return Ok(Self {
                        parent,
                        #[cfg(unix)]
                        name,
                        identity: retained_file_identity(&file)?,
                        file,
                        directory,
                        published: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "staging names collided",
        ))
    }

    fn verify(&self) -> io::Result<()> {
        if retained_file_identity(&self.file)? != self.identity {
            return Err(io::Error::other("staging handle changed"));
        }
        platform::verify_stage_entry(self)
    }

    fn publish(&mut self, leaf: &OsStr) -> Result<(), DurableReplaceError> {
        validate_leaf(leaf).map_err(before)?;
        self.verify().map_err(before)?;
        platform::flush_staging(&self.file, self.directory).map_err(before)?;
        platform::retry(|| {
            self.verify()?;
            platform::rename(self, leaf, false)
        })
        .map_err(before)?;
        self.published = true;
        Ok(())
    }

    fn flush_publication(&self) -> Result<(), DurableReplaceError> {
        platform::flush_publication(self).map_err(after)
    }
}

fn random_name(prefix: &str) -> io::Result<String> {
    use std::fmt::Write as _;
    let mut random = [0u8; 16];
    platform::random(&mut random)?;
    let mut name = prefix.to_owned();
    for byte in random {
        write!(name, "{byte:02x}").expect("writing to a String can't fail");
    }
    Ok(name)
}

impl Drop for Stage<'_> {
    fn drop(&mut self) {
        if !self.published && self.verify().is_ok() {
            let _ = platform::remove_stage(self);
        }
    }
}

/// Verify that a retained directory permits creating and removing a private
/// staging file. Both operations must succeed on Unix and Windows; a
/// read-only filesystem or unsupported operation returns its error. Neither
/// operation resolves the directory's configured path or touches another leaf.
pub fn probe_retained_directory_write(directory: &File) -> io::Result<()> {
    let mut probe = Stage::create(directory, false, false)?;
    probe.verify()?;
    platform::remove_stage(&probe)?;
    probe.published = true;
    Ok(())
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn validate_leaf(leaf: &OsStr) -> io::Result<()> {
    let mut components = Path::new(leaf).components();
    if !matches!(components.next(), Some(Component::Normal(name)) if name == leaf)
        || components.next().is_some()
        || leaf.as_encoded_bytes().contains(&0)
        || (cfg!(windows) && leaf.as_encoded_bytes().contains(&b':'))
    {
        return Err(invalid("expected exactly one ordinary path component"));
    }
    Ok(())
}

fn before(error: io::Error) -> DurableReplaceError {
    DurableReplaceError::new(error, false)
}

fn after(error: io::Error) -> DurableReplaceError {
    DurableReplaceError::new(error, true)
}

#[cfg(test)]
mod tests;
