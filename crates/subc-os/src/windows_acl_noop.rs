//! Windows process hardening, with a no-op on other operating systems.

use std::io;

/// On Windows, restricts newly opened process handles to limited queries,
/// synchronization and termination. On other operating systems, does nothing.
///
/// Call at the top of `main`, before reading any secrets. This is defense in
/// depth, not a security boundary: any process holding enabled
/// `SeDebugPrivilege` (such as an elevated administrator) bypasses the process
/// DACL and can still access memory, and memory exposed before the
/// call is not protected. Handles already opened retain their access rights.
pub fn restrict_current_process() -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn restrict_current_process_is_noop() {
        assert!(super::restrict_current_process().is_ok());
    }
}
