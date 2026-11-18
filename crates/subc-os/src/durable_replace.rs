//! Namespace durability for an already-synced, same-directory staging file.

use std::{io, path::Path};

/// A replacement can be visible even when flushing its publication fails.
#[derive(Debug)]
pub struct DurableReplaceError {
    renamed: bool,
    source: io::Error,
}

impl DurableReplaceError {
    /// Whether the replacement happened, or may have happened when Windows
    /// cannot establish the namespace state after an operating-system error.
    pub fn renamed(&self) -> bool {
        self.renamed
    }

    pub(crate) fn new(source: io::Error, renamed: bool) -> Self {
        Self { renamed, source }
    }
}

impl std::fmt::Display for DurableReplaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(f)
    }
}

impl std::error::Error for DurableReplaceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl From<DurableReplaceError> for io::Error {
    fn from(error: DurableReplaceError) -> Self {
        error.source
    }
}

/// Atomically replace `to`, then flush the publication before returning. The
/// caller must sync the source file's contents first. Neither path may be on a
/// different filesystem; there is deliberately no copy/delete fallback.
/// An error retains whether publication occurred before durability failed.
pub fn durable_replace(from: &Path, to: &Path) -> Result<(), DurableReplaceError> {
    durable_replace_impl(from, to, || Ok(()))
}

/// Inject a failure after a real rename. Unix runs the hook before syncing the
/// directory; Windows flushes the publication, then uses the held source file's
/// identity to distinguish a failed rename from a visible replacement whose
/// publication failed to flush.
#[cfg(feature = "test-support")]
pub fn durable_replace_with_rename_hook(
    from: &Path,
    to: &Path,
    after_rename: impl FnOnce() -> io::Result<()>,
) -> Result<(), DurableReplaceError> {
    durable_replace_impl(from, to, after_rename)
}

fn durable_replace_impl(
    from: &Path,
    to: &Path,
    after_rename: impl FnOnce() -> io::Result<()>,
) -> Result<(), DurableReplaceError> {
    #[cfg(unix)]
    {
        std::fs::rename(from, to).map_err(|error| DurableReplaceError::new(error, false))?;
        after_rename().map_err(|error| DurableReplaceError::new(error, true))?;
        let parent = to.parent().ok_or_else(|| {
            DurableReplaceError::new(
                io::Error::new(io::ErrorKind::InvalidInput, "destination has no parent"),
                true,
            )
        })?;
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| DurableReplaceError::new(error, true))
    }
    #[cfg(windows)]
    {
        windows_replace_with_operation(
            from,
            to,
            || windows_move(from, to).and_then(|()| after_rename()),
            windows_path_identity,
        )
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (from, to, after_rename);
        Err(DurableReplaceError::new(
            io::Error::new(
                io::ErrorKind::Unsupported,
                "durable replacement unsupported",
            ),
            false,
        ))
    }
}

#[cfg(windows)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    volume: u32,
    index: u64,
}

#[cfg(windows)]
fn windows_open_identity(path: &Path) -> io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    std::fs::OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_file_identity(file: &std::fs::File) -> io::Result<FileIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    // SAFETY: Every member of this Win32 output structure accepts zero bits.
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: The handle is borrowed from a live file and the output structure
    // remains writable for the call. A failed call never reads its contents.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileIdentity {
        volume: information.dwVolumeSerialNumber,
        index: (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    })
}

#[cfg(windows)]
fn windows_path_identity(path: &Path) -> io::Result<Option<FileIdentity>> {
    match windows_open_identity(path) {
        Ok(file) => windows_file_identity(&file).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn windows_replace_with_operation(
    from: &Path,
    to: &Path,
    operation: impl FnOnce() -> io::Result<()>,
    inspect: impl Fn(&Path) -> io::Result<Option<FileIdentity>>,
) -> Result<(), DurableReplaceError> {
    let source =
        windows_open_identity(from).map_err(|error| DurableReplaceError::new(error, false))?;
    let identity =
        windows_file_identity(&source).map_err(|error| DurableReplaceError::new(error, false))?;
    let result = operation().map_err(|error| {
        let renamed = match (inspect(from), inspect(to)) {
            (Ok(None), Ok(Some(destination))) if destination == identity => true,
            (Ok(Some(original)), Ok(_)) if original == identity => false,
            // Treat an ambiguous outcome as a rename. An unnecessary publication
            // sequence increment only forces re-verification after a durability
            // error; a missed increment would let replacement bytes retain an
            // older approval's authority.
            _ => true,
        };
        DurableReplaceError::new(error, renamed)
    });
    // Retain the original handle until inspection ends so its identity cannot
    // be recycled after the source name disappears.
    drop(source);
    result
}

#[cfg(windows)]
fn windows_move(from: &Path, to: &Path) -> io::Result<()> {
    if windows_posix_move(from, to)? {
        Ok(())
    } else {
        // Older systems and some volumes do not support POSIX replacement.
        // The legacy move is still atomic and write-through, but may refuse
        // a destination with open readers. Preserve that refusal, not a copy.
        windows_legacy_move(from, to)
    }
}

#[cfg(windows)]
fn windows_wide(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    let mut result: Vec<_> = path.as_os_str().encode_wide().collect();
    if result.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"));
    }
    result.push(0);
    Ok(result)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_posix_move(from: &Path, to: &Path) -> io::Result<bool> {
    use std::{
        mem::{offset_of, size_of},
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    };
    use windows_sys::Win32::{
        Foundation::{ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED, GENERIC_WRITE},
        Storage::FileSystem::{
            FileRenameInfoEx, SetFileInformationByHandle, DELETE, FILE_FLAG_WRITE_THROUGH,
            FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        },
        System::WindowsProgramming::{
            FILE_RENAME_FLAG_POSIX_SEMANTICS, FILE_RENAME_FLAG_REPLACE_IF_EXISTS,
        },
    };
    let destination = windows_wide(&std::path::absolute(to)?)?;
    let name_bytes = (destination.len() - 1) * size_of::<u16>();
    let buffer_bytes =
        offset_of!(FILE_RENAME_INFO, FileName) + destination.len() * size_of::<u16>();
    let length = u32::try_from(buffer_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "rename path too long"))?;
    // usize storage supplies FILE_RENAME_INFO's alignment and room for its
    // trailing UTF-16 name, including the terminating NUL.
    let mut buffer = vec![0usize; buffer_bytes.div_ceil(size_of::<usize>())];
    let information = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: The zeroed allocation has the header's alignment and enough bytes
    // for the complete variable-length name; both remain live through the call.
    unsafe {
        (*information).Anonymous.Flags =
            FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS;
        (*information).FileNameLength = name_bytes as u32;
        std::ptr::copy_nonoverlapping(
            destination.as_ptr(),
            (*information).FileName.as_mut_ptr(),
            destination.len(),
        );
    }
    let file = std::fs::OpenOptions::new()
        .access_mode(DELETE | GENERIC_WRITE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_WRITE_THROUGH)
        .open(from)?;
    // Classic replacement refuses even delete-sharing destination readers.
    // POSIX semantics leave those handles on the old object while new opens
    // resolve to the replacement, without closing existing file handles.
    // SAFETY: file owns the source handle and information describes the live,
    // aligned allocation whose byte length is passed to the synchronous API.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileRenameInfoEx,
            information.cast(),
            length,
        )
    } == 0
    {
        let error = io::Error::last_os_error();
        if matches!(
            error.raw_os_error().map(|code| code as u32),
            Some(ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED)
        ) {
            // false means unsupported before any rename. A later flush failure
            // must return its error: the source pathname no longer names the
            // file we renamed, so retrying could replace unrelated bytes.
            return Ok(false);
        }
        return Err(error);
    }
    // Flush the renamed file's data and metadata before reporting publication.
    // The caller keeps a source handle and compares its file id with both
    // pathnames to distinguish a failed rename from a visible replacement.
    file.sync_all()?;
    Ok(true)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_legacy_move(from: &Path, to: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let from = windows_wide(from)?;
    let to = windows_wide(to)?;
    // SAFETY: Both paths are NUL-terminated and live through the call. There
    // are no borrowed handles. WRITE_THROUGH returns only once the move is
    // flushed; Windows has no directory fsync. Failure is never retried using
    // a weaker rename that could report an approval before publication flush.
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Write};
    fn root() -> cortexkit_test_support::ScratchDir {
        cortexkit_test_support::ScratchDir::new("subc-durable-replace")
    }
    #[test]
    fn durable_replace_replaces_existing_file() {
        let root = root();
        let from = root.join("stage");
        let to = root.join("store");
        fs::write(&to, b"previous").unwrap();
        let mut file = fs::File::create(&from).unwrap();
        file.write_all(b"next").unwrap();
        file.sync_all().unwrap();
        drop(file);
        durable_replace(&from, &to).unwrap();
        assert_eq!(fs::read(&to).unwrap(), b"next");
        assert!(!from.exists());
    }

    #[cfg(windows)]
    #[test]
    fn durable_replace_windows_replaces_destination_with_readers_open() {
        use std::{io::Read, os::windows::fs::OpenOptionsExt};
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        let root = root();
        let (from, to) = replacement_files(&root);
        let mut reader = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&to)
            .unwrap();
        let captured = crate::open_regular_nofollow(&to).unwrap();
        durable_replace(&from, &to).unwrap();
        assert_eq!(fs::read(&to).unwrap(), b"next");
        assert!(!from.exists());
        let mut previous = Vec::new();
        reader.read_to_end(&mut previous).unwrap();
        assert_eq!(previous, b"previous");
        assert_ne!(
            crate::retained_file_identity(&captured).unwrap(),
            crate::retained_file_identity(&fs::File::open(&to).unwrap()).unwrap()
        );
    }
    #[test]
    fn durable_replace_missing_source_is_error() {
        let root = root();
        let to = root.join("store");
        fs::write(&to, b"previous").unwrap();
        assert!(!durable_replace(&root.join("absent"), &to)
            .unwrap_err()
            .renamed());
        assert_eq!(fs::read(&to).unwrap(), b"previous");
    }

    fn replacement_files(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let from = root.join("stage");
        let to = root.join("store");
        let mut file = fs::File::create(&from).unwrap();
        file.write_all(b"next").unwrap();
        file.sync_all().unwrap();
        drop(file);
        fs::write(&to, b"previous").unwrap();
        (from, to)
    }

    #[cfg(unix)]
    #[test]
    fn durable_replace_sync_error_after_rename_retains_publication_witness() {
        let root = root();
        let (from, to) = replacement_files(&root);
        let error = durable_replace_impl(&from, &to, || {
            Err(io::Error::other("injected directory sync failure"))
        })
        .unwrap_err();
        assert!(error.renamed(), "{error}");
        assert_eq!(error.to_string(), "injected directory sync failure");
        assert_eq!(fs::read(to).unwrap(), b"next");
        assert!(!from.exists());
    }

    #[cfg(windows)]
    #[test]
    fn durable_replace_windows_error_after_real_move_reports_renamed() {
        let root = root();
        let (from, to) = replacement_files(&root);
        let error = durable_replace_impl(&from, &to, || {
            Err(io::Error::other("injected write-through failure"))
        })
        .unwrap_err();
        assert!(error.renamed(), "{error}");
        assert_eq!(error.to_string(), "injected write-through failure");
        assert_eq!(fs::read(to).unwrap(), b"next");
        assert!(!from.exists());
    }

    #[cfg(windows)]
    #[test]
    fn durable_replace_windows_error_without_move_reports_not_renamed() {
        let root = root();
        let (from, to) = replacement_files(&root);
        let error = windows_replace_with_operation(
            &from,
            &to,
            || Err(io::Error::other("injected pre-move failure")),
            windows_path_identity,
        )
        .unwrap_err();
        assert!(!error.renamed(), "{error}");
        assert_eq!(fs::read(to).unwrap(), b"previous");
        assert_eq!(fs::read(from).unwrap(), b"next");
    }

    #[cfg(windows)]
    #[test]
    fn durable_replace_windows_inspection_failure_advances_conservatively() {
        let root = root();
        let (from, to) = replacement_files(&root);
        let error = windows_replace_with_operation(
            &from,
            &to,
            || Err(io::Error::other("injected move failure")),
            |path| {
                if path == from {
                    windows_path_identity(path)
                } else {
                    Err(io::Error::other("injected destination inspection failure"))
                }
            },
        )
        .unwrap_err();
        assert!(
            error.renamed(),
            "uncertainty must retain the witness: {error}"
        );
        assert_eq!(error.to_string(), "injected move failure");
        assert_eq!(fs::read(to).unwrap(), b"previous");
        assert_eq!(fs::read(from).unwrap(), b"next");
    }
}
