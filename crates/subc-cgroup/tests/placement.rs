#![cfg(target_os = "linux")]

use std::{fs, io, path::Path, time::Duration};

use subc_cgroup::{apply, prepare_current};
use tokio::{process::Command, time::timeout};

fn child_cgroup_path(pid: u32) -> io::Result<String> {
    fs::read_to_string(format!("/proc/{pid}/cgroup"))
}

#[tokio::test]
async fn child_is_placed_in_its_module_cgroup() -> io::Result<()> {
    let Some(placement) = prepare_current()? else {
        eprintln!("SKIP cgroup placement: /sys/fs/cgroup is not delegated to this test process");
        return Ok(());
    };
    let module = placement.module_path("subc-cgroup-placement-test")?;
    let expected = module
        .strip_prefix("/sys/fs/cgroup")
        .expect("placement must live under /sys/fs/cgroup")
        .display()
        .to_string();

    let mut command = Command::new("sleep");
    command.arg("30");
    apply(&mut command, &module)?;
    let mut child = command.spawn()?;
    let pid = child.id().expect("spawned child has a pid");
    let cgroup = child_cgroup_path(pid)?;
    child.start_kill()?;
    timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("child must exit after kill")?;

    assert!(
        cgroup.lines().any(|line| line == format!("0::/{expected}")),
        "expected child {pid} in {expected}, got {cgroup:?}"
    );
    Ok(())
}

#[tokio::test]
async fn failed_parent_open_reports_the_cgroup_procs_path() {
    let path = Path::new("/definitely-missing-subc-cgroup");
    let mut command = Command::new("true");

    let error =
        apply(&mut command, path).expect_err("a failed cgroup.procs open must fail before spawn");

    assert!(
        error
            .to_string()
            .contains("/definitely-missing-subc-cgroup/cgroup.procs"),
        "parent-side cgroup open failure must name cgroup.procs: {error}"
    );
}
