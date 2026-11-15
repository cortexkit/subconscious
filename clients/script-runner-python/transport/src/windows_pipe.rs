//! The Windows half of the script channel: a named pipe that only the daemon may serve.
//!
//! The pipe name must have the daemon's `subc-launch-<32 hex>` form, and the process serving it
//! must be the daemon whose PID arrived in `SUBC_DAEMON_PID`. The pipe is opened at the
//! identification impersonation level, so the daemon can check who connected without being able
//! to act as this process.

use std::ffi::c_void;
use std::io::{self, Read};

use crate::Refused;

type Handle = *mut c_void;

const GENERIC_READ: u32 = 0x8000_0000;
const OPEN_EXISTING: u32 = 3;
const SECURITY_SQOS_PRESENT: u32 = 0x0010_0000;
const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;
const FILE_TYPE_PIPE: u32 = 3;
const ERROR_BROKEN_PIPE: u32 = 109;
const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *mut c_void,
        disposition: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn GetFileType(handle: Handle) -> u32;
    fn GetNamedPipeServerProcessId(handle: Handle, pid: *mut u32) -> i32;
    fn ReadFile(
        handle: Handle,
        buffer: *mut u8,
        length: u32,
        read: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn GetLastError() -> u32;
    fn CloseHandle(handle: Handle) -> i32;
}

fn refused(reason: &str) -> Refused {
    Refused(format!("script_pipe_refused: {reason}"))
}

/// Checks the pipe name and daemon PID the way the Bun runner does.
pub fn valid_identity(name: &str, daemon_pid: Option<&str>) -> bool {
    let valid_name = name
        .strip_prefix(r"\\.\pipe\subc-launch-")
        .is_some_and(|id| {
            id.len() == 32
                && id
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        });
    let valid_pid = daemon_pid.is_some_and(|pid| {
        !pid.is_empty() && !pid.starts_with('0') && pid.bytes().all(|byte| byte.is_ascii_digit())
    });
    valid_name && valid_pid
}

/// An open, verified pipe from the daemon.
pub struct Pipe(Handle);

impl Drop for Pipe {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateFileW and is closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

impl Read for Pipe {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut count = 0u32;
        let length = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        // SAFETY: the buffer is valid for `length` bytes and the call is synchronous.
        if unsafe {
            ReadFile(
                self.0,
                buffer.as_mut_ptr(),
                length,
                &mut count,
                std::ptr::null_mut(),
            )
        } == 0
        {
            // SAFETY: reads this thread's last error code.
            if unsafe { GetLastError() } == ERROR_BROKEN_PIPE {
                return Ok(0);
            }
            return Err(io::Error::last_os_error());
        }
        Ok(count as usize)
    }
}

/// Opens the named pipe and checks that the daemon process is the one serving it.
pub fn open(name: &str, daemon_pid: Option<&str>) -> Result<Pipe, Refused> {
    if !valid_identity(name, daemon_pid) {
        return Err(refused("invalid pipe name or daemon PID"));
    }
    let expected: u64 = daemon_pid
        .and_then(|pid| pid.parse().ok())
        .ok_or_else(|| refused("invalid pipe name or daemon PID"))?;
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call; the other pointers are null.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            0,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(refused("could not open pipe"));
    }
    let pipe = Pipe(handle);
    let mut server = 0u32;
    // SAFETY: the handle is open and `server` is a valid out pointer.
    let is_pipe = unsafe { GetFileType(pipe.0) } == FILE_TYPE_PIPE;
    // SAFETY: as above.
    let named = is_pipe && unsafe { GetNamedPipeServerProcessId(pipe.0, &mut server) } != 0;
    if !named || u64::from(server) != expected {
        return Err(refused("wrong pipe server"));
    }
    Ok(pipe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{frame, read_framed};

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            instances: u32,
            out_size: u32,
            in_size: u32,
            timeout: u32,
            security: *mut c_void,
        ) -> Handle;
        fn ConnectNamedPipe(handle: Handle, overlapped: *mut c_void) -> i32;
        fn WriteFile(
            handle: Handle,
            buffer: *const u8,
            length: u32,
            written: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        fn FlushFileBuffers(handle: Handle) -> i32;
    }

    const PIPE_ACCESS_OUTBOUND: u32 = 2;

    struct Server(Handle);
    // SAFETY: a pipe handle may be used from any thread.
    unsafe impl Send for Server {}

    fn pipe_name(tag: u32) -> String {
        format!(
            r"\\.\pipe\subc-launch-{:08x}{:024x}",
            std::process::id(),
            tag
        )
    }

    fn serve(name: &str, payload: Vec<u8>) -> std::thread::JoinHandle<()> {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `wide` is NUL-terminated; default security.
        let handle = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_OUTBOUND,
                0,
                1,
                65536,
                65536,
                0,
                std::ptr::null_mut(),
            )
        };
        assert!(handle != INVALID_HANDLE_VALUE, "CreateNamedPipeW failed");
        let server = Server(handle);
        std::thread::spawn(move || {
            let server = server;
            // SAFETY: the handle is a pipe server created above and closed once at the end.
            unsafe {
                ConnectNamedPipe(server.0, std::ptr::null_mut());
                let mut written = 0u32;
                WriteFile(
                    server.0,
                    payload.as_ptr(),
                    payload.len() as u32,
                    &mut written,
                    std::ptr::null_mut(),
                );
                FlushFileBuffers(server.0);
                CloseHandle(server.0);
            }
        })
    }

    #[test]
    fn reads_a_frame_from_the_daemon_pipe() {
        let name = pipe_name(1);
        let writer = serve(&name, frame(b"print('ok')\n"));
        let mut pipe = open(&name, Some(&std::process::id().to_string())).expect("pipe opens");
        assert_eq!(read_framed(&mut pipe), Ok(b"print('ok')\n".to_vec()));
        writer.join().expect("writer");
    }

    #[test]
    fn refuses_a_pipe_served_by_another_process() {
        let name = pipe_name(2);
        let writer = serve(&name, frame(b"print('ok')\n"));
        let other = (std::process::id() + 1).to_string();
        assert_eq!(
            open(&name, Some(&other)).err(),
            Some(refused("wrong pipe server"))
        );
        // The refused client closed its end, so the server's write fails and its thread ends.
        writer.join().expect("writer");
    }

    #[test]
    fn refuses_other_names_and_pids() {
        let pid = std::process::id().to_string();
        let good = r"\\.\pipe\subc-launch-0123456789abcdef0123456789abcdef";
        assert!(valid_identity(good, Some(&pid)));
        for name in [
            r"\\.\pipe\other-0123456789abcdef0123456789abcdef",
            r"\\.\pipe\subc-launch-0123456789ABCDEF0123456789abcdef",
            r"\\.\pipe\subc-launch-0123",
        ] {
            assert_eq!(
                open(name, Some(&pid)).err(),
                Some(refused("invalid pipe name or daemon PID")),
                "{name}"
            );
        }
        for daemon in [None, Some(""), Some("0"), Some("012"), Some("12a")] {
            assert!(!valid_identity(good, daemon), "{daemon:?}");
        }
    }
}
