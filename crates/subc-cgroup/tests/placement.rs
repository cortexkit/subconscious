#![cfg(target_os = "linux")]

use std::{fs, io, path::Path};
use subc_test_support::TestTempDir;

use subc_cgroup::{apply, prepare_at};
use tokio::process::Command;

struct ScratchRoot(TestTempDir);

impl ScratchRoot {
    fn new(name: &str) -> io::Result<Self> {
        let path = TestTempDir::new(&format!("subc-cgroup-{name}"));
        fs::write(path.join("cgroup.procs"), b"")?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

#[tokio::test]
async fn placement_uses_an_explicit_scratch_root() -> io::Result<()> {
    let root = ScratchRoot::new("explicit-root")?;
    let placement = prepare_at(root.path())?.expect("scratch root has a cgroup.procs marker");
    let module = placement.module_path("subc-cgroup-placement-test")?;
    let cgroup_procs = module.join("cgroup.procs");
    fs::write(&cgroup_procs, b"")?;

    let mut command = Command::new("true");
    apply(&mut command, &module)?;
    let status = command.status().await?;

    assert!(
        status.success(),
        "scratch placement child must exit cleanly"
    );
    assert_eq!(
        fs::read(&cgroup_procs)?,
        b"0",
        "the pre-exec placement operation must target the explicit root"
    );
    Ok(())
}

#[test]
fn removal_deletes_an_empty_module_cgroup() -> io::Result<()> {
    let root = ScratchRoot::new("remove-empty")?;
    let placement = prepare_at(root.path())?.expect("scratch root has a cgroup.procs marker");
    let module = placement.module_path("empty-module")?;

    placement.remove_module("empty-module")?;

    assert!(!module.exists(), "empty module cgroup must be removed");
    Ok(())
}

#[test]
fn removal_reports_a_non_empty_module_cgroup() -> io::Result<()> {
    let root = ScratchRoot::new("remove-non-empty")?;
    let placement = prepare_at(root.path())?.expect("scratch root has a cgroup.procs marker");
    let module = placement.module_path("surviving-module")?;
    fs::write(module.join("surviving-process"), b"still present")?;

    let error = placement
        .remove_module("surviving-module")
        .expect_err("a non-empty module cgroup must not be reported as removed");

    assert!(
        module.exists(),
        "failed removal must leave the cgroup intact"
    );
    assert_ne!(
        error.kind(),
        io::ErrorKind::NotFound,
        "the removal error must describe the non-empty cgroup"
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
