//! Atomic publication of a single leaf relative to a retained directory.

use crate::{retained_file_identity, DurableReplaceError, RetainedFileIdentity};
use std::{
    ffi::OsStr,
    fs::File,
    io,
    path::{Component, Path},
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
        use std::fmt::Write as _;

        platform::ensure_directory(parent)?;
        for _ in 0..8 {
            let mut random = [0u8; 16];
            platform::random(&mut random)?;
            let mut name = String::from(".subc-stage-");
            for byte in random {
                write!(name, "{byte:02x}").expect("writing to a String can't fail");
            }
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

impl Drop for Stage<'_> {
    fn drop(&mut self) {
        if !self.published && self.verify().is_ok() {
            platform::remove_stage(self);
        }
    }
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
