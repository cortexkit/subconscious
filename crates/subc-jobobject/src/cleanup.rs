use std::io;

/// Keep ownership of a suspended child until setup succeeds or cleanup completes.
pub(crate) fn start<C>(
    mut child: C,
    setup: impl FnOnce(&C) -> io::Result<()>,
    terminate: impl FnOnce(&mut C) -> io::Result<()>,
) -> io::Result<C> {
    if let Err(error) = setup(&child) {
        if let Err(cleanup) = terminate(&mut child) {
            return Err(io::Error::new(
                error.kind(),
                format!("{error}; suspended child cleanup failed: {cleanup}"),
            ));
        }
        return Err(error);
    }
    Ok(child)
}
