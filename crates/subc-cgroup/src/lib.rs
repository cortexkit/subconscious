#![cfg(target_os = "linux")]
#![deny(unsafe_code)]

use std::{
    fmt::Write as _,
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use tokio::process::Command;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const MODULES_DIR: &str = "subc-modules";
static PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A daemon-owned cgroup subtree prepared during startup.
#[derive(Debug, Clone)]
pub struct Placement {
    modules: PathBuf,
}

impl Placement {
    /// Create or reopen this module's cgroup beneath the delegated subtree.
    pub fn module_path(&self, module_id: &str) -> io::Result<PathBuf> {
        let path = self.modules.join(module_directory_name(module_id));
        match fs::create_dir(&path) {
            Ok(()) => Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(path),
            Err(error) => Err(error),
        }
    }
}

/// Prepare a cgroup subtree for this daemon, or report that it was not delegated.
pub fn prepare_current() -> io::Result<Option<Placement>> {
    let cgroup = current_cgroup_path()?;
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

fn module_directory_name(module_id: &str) -> String {
    let mut name = String::with_capacity(module_id.len());
    for byte in module_id.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            name.push(char::from(byte));
        } else {
            let _ = write!(name, "_{byte:02x}");
        }
    }
    name
}
