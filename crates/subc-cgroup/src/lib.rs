#![cfg(target_os = "linux")]
#![deny(unsafe_code)]

use std::{
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use tokio::process::Command;

mod name;
use name::module_directory_name;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const MODULES_DIR: &str = "subc-modules";
static PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A daemon-owned cgroup subtree prepared during startup.
#[derive(Debug, Clone)]
pub struct Placement {
    modules: PathBuf,
}

/// Result of attempting an atomic module-tree kill.
#[derive(Debug)]
pub enum KillOutcome {
    Killed,
    NotPlaced,
    Unsupported,
    IoError { path: PathBuf, error: io::Error },
}

/// Kill a placed module's entire subtree without enumerating processes.
///
/// Opening without creating the interface preserves compatibility with kernels
/// before 5.14, where `cgroup.kill` is absent.
pub fn kill_module(placement: Option<&Placement>, module_id: &str) -> KillOutcome {
    let Some(placement) = placement else {
        return KillOutcome::NotPlaced;
    };
    let name = module_directory_name(module_id);
    let path = placement.modules.join(&name).join("cgroup.kill");
    // Dot components would target the containing subtree rather than one module.
    if matches!(module_id, "" | "." | "..") {
        return KillOutcome::IoError {
            path,
            error: io::Error::new(
                io::ErrorKind::InvalidInput,
                "module id must name a child cgroup",
            ),
        };
    }
    let module = placement.modules.join(name);
    match fs::metadata(&module) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return KillOutcome::NotPlaced,
        Err(error) => return KillOutcome::IoError { path, error },
        Ok(_) => {}
    }
    let mut file = match fs::OpenOptions::new().write(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return KillOutcome::Unsupported,
        Err(error) => return KillOutcome::IoError { path, error },
    };
    match file.write_all(b"1") {
        Ok(()) => KillOutcome::Killed,
        Err(error) => KillOutcome::IoError { path, error },
    }
}

impl Placement {
    /// Create or reopen this module's cgroup beneath the delegated subtree.
    pub fn module_path(&self, module_id: &str) -> io::Result<PathBuf> {
        if matches!(module_id, "" | "." | "..") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "module id must name a child cgroup",
            ));
        }
        let path = self.modules.join(module_directory_name(module_id));
        match fs::create_dir(&path) {
            Ok(()) => Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(path),
            Err(error) => Err(error),
        }
    }

    /// Remove this module's cgroup after its child has been reaped.
    ///
    /// The kernel refuses to remove a cgroup that still contains a process. The
    /// caller must report that error rather than treating the module as cleaned up.
    pub fn remove_module(&self, module_id: &str) -> io::Result<()> {
        fs::remove_dir(self.modules.join(module_directory_name(module_id)))
    }
}

/// Prepare the current process's cgroup subtree, or report that it was not delegated.
pub fn prepare_current() -> io::Result<Option<Placement>> {
    prepare_at(&current_cgroup_path()?)
}

/// Prepare a cgroup subtree beneath an explicit root.
///
/// Explicit roots let tests and embedded daemons own the location they reconcile
/// instead of deriving a production location from the calling process.
pub fn prepare_at(cgroup: &Path) -> io::Result<Option<Placement>> {
    if !cgroup.join("cgroup.procs").is_file() {
        return Ok(None);
    }

    let modules = cgroup.join(MODULES_DIR);
    match fs::create_dir(&modules) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Ok(None),
        Err(error) => return Err(error),
    }

    let probe = modules.join(format!(
        ".subc-delegation-probe-{}-{}",
        std::process::id(),
        PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    match fs::create_dir(&probe) {
        Ok(()) => fs::remove_dir(&probe)?,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Ok(None),
        Err(error) => return Err(error),
    }

    Ok(Some(Placement { modules }))
}

/// Open the module's cgroup in the parent and build a child-side placement operation.
pub fn place_in(path: &Path) -> io::Result<impl FnMut() -> io::Result<()> + Send + Sync> {
    let cgroup_procs = path.join("cgroup.procs");
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&cgroup_procs)
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "failed to open cgroup.procs '{}': {error}",
                    cgroup_procs.display()
                ),
            )
        })?;
    Ok(move || (&file).write_all(b"0"))
}

/// Arrange for a child process to enter `path` immediately before exec.
#[allow(unsafe_code)]
pub fn apply(cmd: &mut Command, path: &Path) -> io::Result<()> {
    let place_in = place_in(path)?;
    // The child closure only calls write(2) on the inherited descriptor. The open and
    // path conversion happen in the parent, so allocation is impossible by construction,
    // not by path length, between fork and exec.
    unsafe {
        cmd.pre_exec(place_in);
    }
    Ok(())
}

fn current_cgroup_path() -> io::Result<PathBuf> {
    let cgroups = fs::read_to_string("/proc/self/cgroup")?;
    let relative = cgroups
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "cgroup v2 is unavailable"))?;
    let relative = relative.strip_prefix('/').unwrap_or(relative);
    Ok(Path::new(CGROUP_ROOT).join(relative))
}
