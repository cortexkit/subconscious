#![allow(unsafe_code)]

use std::io;

use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

/// Restore Ctrl-C delivery in the test launcher and the children it starts.
///
/// Windows children inherit the "ignore Ctrl-C" setting from their parent, such
/// as a CI runner. A launcher in a fresh pseudo-console needs to clear it before
/// spawning `ck` so a Ctrl-C keystroke reaches the real console handler.
pub fn restore_ctrl_c() -> io::Result<()> {
    // SAFETY: A null handler with FALSE only clears the current process's
    // ignore flag. There is no callback or borrowed memory.
    if unsafe { SetConsoleCtrlHandler(None, 0) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
