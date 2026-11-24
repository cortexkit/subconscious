#[cfg(target_os = "linux")]
use std::fs;
use std::{collections::BTreeMap, path::Path, time::Duration};

use subc_client_rs::{ConsumerOptions, SubcConsumer, SupervisorListResponse};
use subc_control::SupervisorHealthEntry;
use subc_daemon::{LiveChild, LiveChildrenRoster, RecordReadError};

use super::{Stack, SurvivorReason};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProcessReading {
    Gone,
    Alive(u64),
    Unreadable,
}

pub(super) trait Host {
    fn list(&mut self, limit: Duration) -> Option<SupervisorListResponse>;
    fn cached_health(&mut self, limit: Duration) -> Vec<SupervisorHealthEntry>;
    fn connects(&mut self, limit: Duration) -> bool;
    fn roster(&mut self) -> Result<Option<LiveChildrenRoster>, RecordReadError>;
    fn configured(&mut self) -> Result<BTreeMap<String, bool>, String>;
    fn process(&mut self, pid: u32) -> ProcessReading;
    fn daemon_process(&mut self, pid: u32) -> ProcessReading;
    fn executable(&mut self, child: &LiveChild) -> Option<SurvivorReason>;
    fn same_boot(&mut self, child: &LiveChild) -> Option<bool>;
    fn container_empty(&mut self, child: &LiveChild) -> bool;
}

pub(super) struct SystemHost<'a> {
    pub stack: &'a Stack,
    pub runtime: tokio::runtime::Runtime,
}

impl SystemHost<'_> {
    async fn connect(connection: Option<&Path>) -> Option<SubcConsumer> {
        let found = subc_transport::connection_file::discover(connection).ok()?;
        SubcConsumer::connect(&found.path, ConsumerOptions::default())
            .await
            .ok()
    }
}

impl Host for SystemHost<'_> {
    fn list(&mut self, limit: Duration) -> Option<SupervisorListResponse> {
        self.runtime.block_on(async {
            tokio::time::timeout(limit, async {
                Self::connect(self.stack.connection.as_deref())
                    .await?
                    .supervisor_list()
                    .await
                    .ok()
            })
            .await
            .ok()
            .flatten()
        })
    }

    fn cached_health(&mut self, limit: Duration) -> Vec<SupervisorHealthEntry> {
        self.runtime.block_on(async {
            tokio::time::timeout(limit, async {
                Self::connect(self.stack.connection.as_deref())
                    .await?
                    .supervisor_health()
                    .await
                    .ok()
            })
            .await
            .ok()
            .flatten()
            .map_or_else(Vec::new, |health| health.modules)
        })
    }

    fn connects(&mut self, limit: Duration) -> bool {
        self.runtime.block_on(async {
            tokio::time::timeout(limit, Self::connect(self.stack.connection.as_deref()))
                .await
                .is_ok_and(|connected| connected.is_some())
        })
    }

    fn roster(&mut self) -> Result<Option<LiveChildrenRoster>, RecordReadError> {
        let run = subc_daemon::daemon_config::daemon_run_dir()
            .map_err(|error| RecordReadError::Io(std::io::Error::other(error)))?;
        subc_daemon::read_live_children_roster(&run.join("live-children.json"))
    }

    fn configured(&mut self) -> Result<BTreeMap<String, bool>, String> {
        Ok(subc_daemon::daemon_config::load(&self.stack.config)
            .map_err(|error| error.to_string())?
            .into_iter()
            .flat_map(|config| config.modules)
            .map(|module| (module.module_id, module.health.http.is_some()))
            .collect())
    }

    fn process(&mut self, pid: u32) -> ProcessReading {
        process(pid)
    }

    fn daemon_process(&mut self, pid: u32) -> ProcessReading {
        let reading = process(pid);
        let ProcessReading::Alive(native) = reading else {
            return reading;
        };
        let millis = subc_os::process_start_time_ms(pid);
        match process(pid) {
            ProcessReading::Gone => ProcessReading::Gone,
            ProcessReading::Alive(after) if after == native => {
                millis.map_or(ProcessReading::Unreadable, ProcessReading::Alive)
            }
            _ => ProcessReading::Unreadable,
        }
    }

    fn executable(&mut self, child: &LiveChild) -> Option<SurvivorReason> {
        executable(child)
    }

    fn same_boot(&mut self, child: &LiveChild) -> Option<bool> {
        same_boot(child)
    }

    fn container_empty(&mut self, child: &LiveChild) -> bool {
        container_empty(child)
    }
}

pub(super) fn roster_liveness(host: &mut impl Host, child: &LiveChild) -> ProcessReading {
    let boot = host.same_boot(child);
    if boot == Some(false) {
        return ProcessReading::Gone;
    }
    match (host.process(child.pid), child.start_time) {
        (ProcessReading::Alive(now), Some(recorded)) if now != recorded => ProcessReading::Gone,
        (ProcessReading::Alive(_), None) => ProcessReading::Unreadable,
        (ProcessReading::Alive(_), _) if boot.is_none() => ProcessReading::Unreadable,
        (reading, _) => reading,
    }
}

pub(crate) fn process(pid: u32) -> ProcessReading {
    if pid == 0 {
        return ProcessReading::Unreadable;
    }
    #[cfg(unix)]
    if pid > 1 {
        // The kernel inspection distinguishes absence from a failed read on
        // Darwin, where Process::open alone collapses those two cases.
        return match subc_os::process_group::inspect(pid) {
            Ok(Some(identity)) if !identity.exited => ProcessReading::Alive(identity.start_time),
            #[cfg(target_os = "linux")]
            Ok(None)
                if subc_os::Process::open(pid)
                    .ok()
                    .flatten()
                    .is_some_and(|process| process.is_alive_but_unobservable()) =>
            {
                ProcessReading::Unreadable
            }
            Ok(_) => ProcessReading::Gone,
            Err(_) => ProcessReading::Unreadable,
        };
    }
    match subc_os::Process::open(pid) {
        Ok(None) if cfg!(windows) => ProcessReading::Gone,
        Ok(Some(process)) => process
            .observe()
            .map_or(ProcessReading::Unreadable, |value| {
                ProcessReading::Alive(value.start_time)
            }),
        _ => ProcessReading::Unreadable,
    }
}

fn executable(child: &LiveChild) -> Option<SurvivorReason> {
    use SurvivorReason::{ExecutableChanged, IdentityUnreadable};
    let Some(recorded) = child.executable else {
        return Some(IdentityUnreadable);
    };
    let Some(image) = subc_os::Process::open(child.pid)
        .ok()
        .flatten()
        .and_then(|process| process.observe())
        .filter(|observed| Some(observed.start_time) == child.start_time)
        .and_then(|observed| observed.executable)
    else {
        // Windows has no read of the image's file ID from a live process.
        return Some(IdentityUnreadable);
    };
    if (recorded.device, recorded.inode) != (image.device, image.inode) {
        return Some(ExecutableChanged);
    }
    #[cfg(target_os = "linux")]
    {
        // procfs can still stat the old inode after replacement. Even then,
        // an executable removed from its path doesn't prove the PID exited.
        let Ok(path) = fs::read_link(format!("/proc/{}/exe", child.pid)) else {
            return Some(IdentityUnreadable);
        };
        if path.as_os_str().as_encoded_bytes().ends_with(b" (deleted)") {
            return Some(ExecutableChanged);
        }
        match subc_os::file_identity(&path) {
            None => return Some(IdentityUnreadable),
            Some(current) if current != image => return Some(ExecutableChanged),
            _ => {}
        }
    }
    None
}

fn same_boot(child: &LiveChild) -> Option<bool> {
    if cfg!(unix) {
        if let Some(boot) = child
            .container
            .as_ref()
            .and_then(|container| container.boot_id.as_deref())
        {
            return subc_os::boot_id::is_current(boot).ok();
        }
    }
    Some(true)
}

fn container_empty(child: &LiveChild) -> bool {
    if same_boot(child) == Some(false) {
        return true;
    }
    let Some(container) = &child.container else {
        return false;
    };
    if !container
        .boot_id
        .as_deref()
        .is_some_and(|boot| matches!(subc_os::boot_id::is_current(boot), Ok(true)))
    {
        return false;
    }
    #[cfg(target_os = "linux")]
    if let Some(path) = &container.cgroup_path {
        // A missing file is not evidence that the cgroup was emptied. Trust
        // only the kernel's population counter at the recorded cgroup.
        return path.starts_with("/sys/fs/cgroup")
            && !path
                .components()
                .any(|part| part == std::path::Component::ParentDir)
            && fs::read_to_string(path.join("cgroup.events"))
                .is_ok_and(|text| text.lines().any(|line| line == "populated 0"));
    }
    #[cfg(unix)]
    if let (Some(group), Some(session), Some(_)) = (
        container.pgid,
        container.session_id,
        container.created_start_time,
    ) {
        return group > 1
            && subc_os::process_group::pids().is_ok_and(|pids| {
                pids.into_iter()
                    .all(|pid| match subc_os::process_group::inspect(pid) {
                        Ok(None) => true,
                        Ok(Some(identity)) => {
                            identity.exited
                                || identity.pgid != group
                                || identity.session_id != session
                        }
                        Err(_) => false,
                    })
            });
    }
    false
}
