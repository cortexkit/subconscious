use std::process::Command;

#[test]
fn a_child_reads_its_actual_parent_pid() {
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "parent_pid_child", "--nocapture"])
        .env("SUBC_OS_REPORT_PARENT", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains(&format!("parent pid: Some({})", std::process::id())));
}

#[test]
fn parent_pid_child() {
    let Ok(_) = std::env::var("SUBC_OS_REPORT_PARENT") else {
        return;
    };
    println!("parent pid: {:?}", subc_os::parent_pid(std::process::id()));
}

#[test]
fn a_missing_pid_has_no_parent() {
    assert_eq!(subc_os::parent_pid(u32::MAX), None);
}
