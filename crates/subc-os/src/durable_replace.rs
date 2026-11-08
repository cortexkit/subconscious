//! Namespace durability for an already-synced, same-directory staging file.

use std::{io, path::Path};

/// Atomically replace `to`, then flush the publication before returning. The
/// caller must sync the source file's contents first. Neither path may be on a
/// different filesystem; there is deliberately no copy/delete fallback.
pub fn durable_replace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::rename(from, to)?;
        let parent = to.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "destination has no parent")
        })?;
        std::fs::File::open(parent)?.sync_all()
    }
    #[cfg(windows)]
    {
        windows_replace(from, to)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (from, to);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "durable replacement unsupported",
        ))
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    fn wide(path: &Path) -> io::Result<Vec<u16>> {
        let mut result: Vec<_> = path.as_os_str().encode_wide().collect();
        if result.contains(&0) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"));
        }
        result.push(0);
        Ok(result)
    }
    let from = wide(from)?;
    let to = wide(to)?;
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
    fn root() -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "subc-durable-replace-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
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
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn durable_replace_missing_source_is_error() {
        let root = root();
        let to = root.join("store");
        fs::write(&to, b"previous").unwrap();
        assert!(durable_replace(&root.join("absent"), &to).is_err());
        assert_eq!(fs::read(&to).unwrap(), b"previous");
        fs::remove_dir_all(root).unwrap();
    }
}
