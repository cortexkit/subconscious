use super::{open_regular_nofollow, OpenRegularError};
use std::{
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "subc-regular-open-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn open_regular_nofollow_accepts_regular_file() {
    let temp = Temp::new();
    let path = temp.0.join("regular");
    std::fs::write(&path, b"regular bytes").unwrap();
    let file = open_regular_nofollow(&path).unwrap();
    assert!(file.metadata().unwrap().is_file());
    assert_eq!(file.metadata().unwrap().len(), 13);
}

#[test]
fn open_regular_nofollow_refuses_directory() {
    let temp = Temp::new();
    assert!(matches!(
        open_regular_nofollow(&temp.0),
        Err(OpenRegularError::NotRegularFile)
    ));
}

#[test]
fn open_regular_nofollow_refuses_leaf_symlink() {
    let temp = Temp::new();
    let path = temp.0.join("regular");
    let link = temp.0.join("link");
    std::fs::write(&path, b"regular bytes").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&path, &link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&path, &link).unwrap();
    assert!(matches!(
        open_regular_nofollow(&link),
        Err(OpenRegularError::Symlink)
    ));
}

#[test]
fn open_regular_nofollow_non_regular_probe() {
    let Some(path) = std::env::var_os("SUBC_REGULAR_OPEN_TEST_INPUT") else {
        return;
    };
    assert!(matches!(
        open_regular_nofollow(std::path::Path::new(&path)),
        Err(OpenRegularError::NotRegularFile)
    ));
}

fn bounded_non_regular_probe(path: &std::path::Path) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "retained_file::open_tests::open_regular_nofollow_non_regular_probe",
        ])
        .env("SUBC_REGULAR_OPEN_TEST_INPUT", path)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "regular-input probe refused incorrectly: {status}"
            );
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("open_regular_nofollow blocked on a non-regular input");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
#[allow(unsafe_code)]
fn open_regular_nofollow_refuses_fifo_without_blocking() {
    use std::os::unix::ffi::OsStrExt;
    let temp = Temp::new();
    let path = temp.0.join("fifo");
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: name is a live NUL-terminated pathname in the owned test directory;
    // mkfifo creates only that test FIFO and mode contains ordinary permission bits.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    bounded_non_regular_probe(&path);
}

#[cfg(windows)]
#[test]
#[allow(unsafe_code)]
fn open_regular_nofollow_refuses_named_pipe_without_blocking() {
    use std::os::windows::{
        ffi::OsStrExt,
        io::{FromRawHandle, OwnedHandle},
    };
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX},
        System::Pipes::{CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT},
    };
    let temp = Temp::new();
    let path = PathBuf::from(format!(
        r"\\.\pipe\subc-regular-open-{}-{}",
        std::process::id(),
        temp.0.file_name().unwrap().to_string_lossy()
    ));
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: name is live and NUL-terminated; scalar arguments describe one
    // test-owned byte pipe. A null security pointer requests default security.
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            512,
            512,
            0,
            std::ptr::null(),
        )
    };
    assert_ne!(
        handle,
        INVALID_HANDLE_VALUE,
        "{}",
        std::io::Error::last_os_error()
    );
    // SAFETY: CreateNamedPipeW returned a newly owned valid handle. OwnedHandle
    // closes it once after the child probe returns, including during unwinding.
    let _server = unsafe { OwnedHandle::from_raw_handle(handle) };
    bounded_non_regular_probe(&path);
}
