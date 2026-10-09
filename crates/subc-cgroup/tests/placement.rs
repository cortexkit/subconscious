#![cfg(target_os = "linux")]

use cortexkit_test_support::ScratchDir;
use std::{fs, io, path::Path};

use subc_cgroup::{apply, prepare_at};
use tokio::process::Command;

struct ScratchRoot(ScratchDir);

impl ScratchRoot {
    fn new(name: &str) -> io::Result<Self> {
        let path = ScratchDir::new(&format!("subc-cgroup-{name}"));
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

#[test]
fn kill_outcomes_distinguish_placement_support_and_write() -> io::Result<()> {
    use subc_cgroup::{kill_module, KillOutcome};
    let root = ScratchRoot::new("kill-outcomes")?;
    let placement = prepare_at(root.path())?.expect("scratch root marker");
    assert!(matches!(
        kill_module(None, "module"),
        KillOutcome::NotPlaced
    ));
    assert!(matches!(
        kill_module(Some(&placement), "module"),
        KillOutcome::NotPlaced
    ));
    let module = placement.module_path("module")?;
    assert!(matches!(
        kill_module(Some(&placement), "module"),
        KillOutcome::Unsupported
    ));
    let path = module.join("cgroup.kill");
    fs::write(&path, b"")?;
    assert!(matches!(
        kill_module(Some(&placement), "module"),
        KillOutcome::Killed
    ));
    assert_eq!(fs::read(&path)?, b"1");
    fs::remove_file(&path)?;
    fs::create_dir(&path)?;
    match kill_module(Some(&placement), "module") {
        KillOutcome::IoError { path: actual, .. } => assert_eq!(actual, path),
        other => panic!("expected path-bearing write error, got {other:?}"),
    }
    Ok(())
}

#[test]
fn kill_refuses_ids_that_target_the_containing_subtree() -> io::Result<()> {
    use subc_cgroup::{kill_module, KillOutcome};
    let root = ScratchRoot::new("kill-invalid-id")?;
    let placement = prepare_at(root.path())?.expect("scratch root marker");
    let parent_kill = root.path().join("cgroup.kill");
    fs::write(&parent_kill, b"untouched")?;
    for id in ["", ".", ".."] {
        assert!(
            matches!(kill_module(Some(&placement), id), KillOutcome::IoError { error, .. } if error.kind() == io::ErrorKind::InvalidInput)
        );
    }
    assert_eq!(fs::read(parent_kill)?, b"untouched");
    Ok(())
}
