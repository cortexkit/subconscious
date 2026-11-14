//! Identity-bearing Unix script transport. The receiver validates before adopting
//! a descriptor because runtimes may reuse inherited descriptor numbers for IPC.

use std::{fs::File, io};

pub const SCRIPT_FD_ENV: &str = "SUBC_SCRIPT_FD";
pub const SCRIPT_FD: i32 = 6;

/// Create close-on-exec sources; only ChildDescriptorSlots makes the reader
/// inheritable in the intended child. Payload writes happen after spawn.
#[cfg(unix)]
pub fn handoff() -> io::Result<(std::os::fd::OwnedFd, File, String)> {
    use std::os::unix::fs::MetadataExt;
    let (reader, writer) = io::pipe()?;
    let reader = crate::registration_channel::outside_child_slots(reader.into())?;
    let file = File::from(reader);
    let inode = file.metadata()?.ino();
    Ok((
        file.into(),
        File::from(std::os::fd::OwnedFd::from(writer)),
        format!("{SCRIPT_FD}:{inode}"),
    ))
}

/// Take a pipe only after its type and inode match the advertised identity.
/// A refusal leaves the descriptor untouched.
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn take(value: &str) -> io::Result<File> {
    use std::os::fd::{BorrowedFd, FromRawFd};
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "invalid script pipe identity");
    let (fd, inode) = value.split_once(':').ok_or_else(invalid)?;
    let fd: i32 = fd.parse().map_err(|_| invalid())?;
    let inode: u64 = inode.parse().map_err(|_| invalid())?;
    if fd < 3 {
        return Err(invalid());
    }
    // SAFETY: borrowing performs no I/O or ownership transfer. fstat checks
    // validity before the descriptor is adopted, as in the nonce reader.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let copy = borrowed.try_clone_to_owned()?;
    let metadata = File::from(copy).metadata()?;
    if !metadata.file_type().is_fifo() || metadata.ino() != inode {
        return Err(invalid());
    }
    // SAFETY: validation succeeded; the caller transfers this descriptor once.
    let file = unsafe { File::from_raw_fd(fd) };
    rustix::io::fcntl_setfd(&file, rustix::io::FdFlags::CLOEXEC)?;
    Ok(file)
}

/// Open only a local named pipe owned by the advertised daemon process.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn connect(name: &str, daemon_pid: u32) -> io::Result<File> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::{
        Storage::FileSystem::{
            GetFileType, FILE_TYPE_PIPE, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
        },
        System::Pipes::GetNamedPipeServerProcessId,
    };
    if !name.starts_with(r"\\.\pipe\") || daemon_pid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid script pipe identity",
        ));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION)
        .open(name)?;
    let mut server = 0;
    // SAFETY: file retains the connected handle and server is writable.
    if unsafe { GetFileType(file.as_raw_handle()) } != FILE_TYPE_PIPE
        || unsafe { GetNamedPipeServerProcessId(file.as_raw_handle(), &mut server) } == 0
        || server != daemon_pid
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "script pipe server is not the daemon",
        ));
    }
    Ok(file)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn script_pipe_handoff_names_fifo_and_keeps_sources_above_reserved_slots() {
        let (reader, writer, metadata) = handoff().unwrap();
        assert!(reader.as_raw_fd() > SCRIPT_FD);
        let file = File::from(reader);
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let stat = file.metadata().unwrap();
        assert!(stat.file_type().is_fifo());
        assert_eq!(metadata, format!("6:{}", stat.ino()));
        drop(writer);
    }

    #[test]
    fn script_pipe_wrong_identity_does_not_take_descriptor() {
        let (reader, writer, _) = handoff().unwrap();
        let fd = reader.as_raw_fd();
        assert!(take(&format!("{fd}:0")).is_err());
        assert!(File::from(reader).metadata().is_ok());
        drop(writer);
    }

    #[test]
    fn script_pipe_slot_six_survives_spawn_and_is_not_runtime_ipc_slot() {
        let (reader, mut writer, metadata) = handoff().unwrap();
        use std::io::Write;
        writer.write_all(b"transport").unwrap();
        drop(writer);
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "read value <&6; test \"$value\" = transport"]);
        let mut slots = crate::registration_channel::ChildDescriptorSlots::default();
        slots.script(reader).unwrap();
        slots.install(&mut command);
        assert!(metadata.starts_with("6:"));
        assert!(command.status().unwrap().success());
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn script_pipe_checks_local_name_and_server_process_before_reading() {
        assert!(connect(r"\\remote\pipe\script", std::process::id()).is_err());
        for correct in [false, true] {
            let pipe = crate::registration_channel::RegistrationPipe::new().unwrap();
            let name = pipe.name().to_owned();
            let pid = std::process::id();
            let server = std::thread::spawn(move || {
                pipe.accept(
                    pid,
                    std::time::Instant::now() + std::time::Duration::from_secs(5),
                )
            });
            let client = connect(&name, if correct { pid } else { pid + 1 });
            assert_eq!(client.is_ok(), correct);
            let accepted = server.join().unwrap();
            if correct {
                assert!(accepted.is_ok());
            }
            drop(client);
            drop(accepted);
        }
    }
}
