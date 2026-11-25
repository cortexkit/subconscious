//! A native exec target used only by the daemon upgrade integration tests.

use std::{ffi::OsString, os::unix::fs::FileTypeExt};

pub(crate) fn run(args: &[OsString]) -> bool {
    let Some(marker) = std::env::var_os("SUBC_TEST_UPGRADE_TARGET_MARKER") else {
        return false;
    };
    if args == ["--build-info"] {
        println!(
            "{}",
            serde_json::json!({"core_version":"1.2.3", "build_git_sha":null, "handover_versions":[2]})
        );
        return true;
    }
    let [handover_flag, path, key_flag, fd] = args else {
        return false;
    };
    if handover_flag != "--resume-handover" || key_flag != "--handover-key-fd" {
        return false;
    }
    let fd = fd
        .to_str()
        .and_then(|fd| fd.parse::<u32>().ok())
        .expect("numeric handover descriptor");
    assert!(std::fs::metadata(path).unwrap().len() > 0);
    assert!(std::fs::metadata(format!("/dev/fd/{fd}"))
        .unwrap()
        .file_type()
        .is_fifo());
    std::fs::write(marker, std::process::id().to_string()).unwrap();
    true
}
