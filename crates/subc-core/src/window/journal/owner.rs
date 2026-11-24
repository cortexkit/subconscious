use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::stack::host::{self, ProcessReading};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub pid: u32,
    pub start_time: u64,
    #[serde(deserialize_with = "super::records::required_option")]
    pub boot: Option<String>,
    #[serde(deserialize_with = "super::records::required_option")]
    pub executable: Option<PathBuf>,
    #[serde(deserialize_with = "super::records::required_option")]
    pub executable_identity: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    Alive,
    Gone,
    Unreadable,
}

impl Owner {
    pub fn same_process(&self, other: &Self) -> bool {
        (self.pid, self.start_time, &self.boot) == (other.pid, other.start_time, &other.boot)
    }

    pub fn describe(&self) -> String {
        let executable = match (&self.executable, &self.executable_identity) {
            (Some(path), _) => path.display().to_string(),
            (None, Some(identity)) => format!("path unavailable, file {identity}"),
            (None, None) => "path unavailable".into(),
        };
        format!(
            "pid {}, start time {}, executable {executable}",
            self.pid, self.start_time
        )
    }

    pub fn liveness(&self) -> Liveness {
        self.compare(boot().as_ref().map_err(|_| ()), || host::process(self.pid))
    }

    pub(crate) fn compare(
        &self,
        boot: Result<&Option<String>, ()>,
        process: impl FnOnce() -> ProcessReading,
    ) -> Liveness {
        if boot.is_ok_and(|boot| boot != &self.boot) {
            return Liveness::Gone;
        }
        match process() {
            ProcessReading::Gone => Liveness::Gone,
            ProcessReading::Alive(start) if start != self.start_time => Liveness::Gone,
            ProcessReading::Alive(_) if boot.is_ok() => Liveness::Alive,
            _ => Liveness::Unreadable,
        }
    }

    pub fn current() -> Result<Self, String> {
        capture(std::process::id())
    }

    pub(super) fn valid(&self) -> bool {
        self.pid != 0
            && self.start_time != 0
            && match &self.boot {
                Some(boot) if cfg!(any(target_os = "linux", target_os = "macos")) => {
                    boot.len() == 36
                        && boot.bytes().enumerate().all(|(i, byte)| {
                            if matches!(i, 8 | 13 | 18 | 23) {
                                byte == b'-'
                            } else {
                                byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()
                            }
                        })
                }
                None => cfg!(not(any(target_os = "linux", target_os = "macos"))),
                _ => false,
            }
    }
}

pub(crate) fn boot() -> Result<Option<String>, String> {
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        subc_os::boot_id::current()
            .map(Some)
            .map_err(|error| format!("boot identity is unreadable: {error}"))
    } else {
        Ok(None)
    }
}

pub(crate) fn capture(pid: u32) -> Result<Owner, String> {
    let unreadable = || format!("process identity for pid {pid} is gone or unreadable");
    let ProcessReading::Alive(start_time) = host::process(pid) else {
        return Err(unreadable());
    };
    let boot = boot()?;
    let executable_identity = subc_os::Process::open(pid)
        .ok()
        .flatten()
        .and_then(|p| p.observe())
        .and_then(|observation| observation.executable)
        .map(|value| format!("{value:?}"));
    #[cfg(target_os = "linux")]
    let executable = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
    #[cfg(not(target_os = "linux"))]
    let executable = None;
    if host::process(pid) != ProcessReading::Alive(start_time) {
        return Err(unreadable());
    }
    Ok(Owner {
        pid,
        start_time,
        boot,
        executable,
        executable_identity,
    })
}

pub(crate) fn ancestor(pid: u32) -> Result<Owner, String> {
    let owner = capture(pid)?;
    let mut child = std::process::id();
    let mut visited = std::collections::HashSet::new();
    while visited.insert(child) {
        let Some(parent) = subc_os::parent_pid(child).filter(|parent| *parent > 0) else {
            break;
        };
        if parent == pid && pid != std::process::id() && owner.liveness() == Liveness::Alive {
            return Ok(owner);
        }
        child = parent;
    }
    Err(format!("owner pid {pid} is not a readable ancestor of ck"))
}
