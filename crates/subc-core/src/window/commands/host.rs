use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use subc_client_rs::{ConsumerOptions, SubcConsumer, SupervisorListResponse};
use subc_control::RunningImageAgreement;
use subc_transport::connection_file;

use super::super::{
    journal::{owner, Liveness, Owner},
    Windows,
};
use crate::{
    setup::{
        self,
        runtime::{self, RuntimePlatform, SystemCommandRunner},
    },
    stack,
};
use stack::host::{self, ProcessReading};

const QUERY: Duration = Duration::from_secs(2);

pub(super) trait Host {
    fn stamped(&mut self) -> Result<Option<bool>, String>;
    fn owner(&self, pid: u32) -> Result<Owner, String>;
    fn liveness(&self, owner: &Owner) -> Liveness;
    fn manager_live(&mut self) -> Result<bool, String>;
    fn daemon_path(&self) -> Result<PathBuf, String>;
    fn daemon(&mut self) -> Result<Option<Owner>, String>;
    fn module(&mut self, id: &str) -> Result<ModuleReading, String>;
    fn has_running_image_proof(&self) -> bool;
    fn image_matches(&self, daemon: &Owner, path: &Path) -> bool;
}

pub(super) struct ModuleReading {
    pub generation: u64,
    pub process: Option<Owner>,
    pub image_disagrees: bool,
}

pub(super) struct SystemHost<'a>(pub &'a Windows);

fn executor() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())
}

impl SystemHost<'_> {
    fn list(&self) -> Result<SupervisorListResponse, String> {
        executor()?.block_on(async {
            tokio::time::timeout(QUERY, async {
                let found = connection_file::discover(self.0.connection.as_deref())
                    .map_err(|error| error.to_string())?;
                let client = SubcConsumer::connect(&found.path, ConsumerOptions::default())
                    .await
                    .map_err(|error| error.to_string())?;
                client
                    .supervisor_list()
                    .await
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|_| "daemon did not answer supervisor.list".to_string())?
        })
    }
}

impl Host for SystemHost<'_> {
    fn stamped(&mut self) -> Result<Option<bool>, String> {
        executor()?.block_on(async {
            let Ok(found) = connection_file::discover(self.0.connection.as_deref()) else {
                return Ok(None);
            };
            let local = tokio::time::timeout(
                QUERY,
                SubcConsumer::connect_local(&found.path, ConsumerOptions::default()),
            )
            .await
            .map_err(|_| "timed out connecting to the daemon's local endpoint".to_owned())
            .and_then(|result| result.map_err(|error| error.to_string()));
            let client = match local {
                Ok(client) => client,
                Err(error) => {
                    // A live daemon without an owner connection is not offline.
                    // TCP can prove reachability, but cannot query protection.
                    if matches!(
                        tokio::time::timeout(
                            QUERY,
                            SubcConsumer::connect(&found.path, ConsumerOptions::default()),
                        )
                        .await,
                        Ok(Ok(_))
                    ) {
                        return Err(error);
                    }
                    return Ok(None);
                }
            };
            // The owner connection opened, so the daemon is live: a late answer
            // can't be read as "offline", which would skip the protection check.
            let Ok(status) = tokio::time::timeout(QUERY, client.config_status()).await else {
                return Err("daemon did not answer config.status within 2 s".to_owned());
            };
            let status = status.map_err(|error| error.to_string())?;
            match status.state.as_str() {
                "stamped" => Ok(Some(true)),
                "unstamped" if !status.anchor_pending => Ok(Some(false)),
                state => Err(format!(
                    "configuration protection could not be confirmed: {state:?}"
                )),
            }
        })
    }

    fn owner(&self, pid: u32) -> Result<Owner, String> {
        owner::ancestor(pid)
    }
    fn liveness(&self, owner: &Owner) -> Liveness {
        owner.liveness()
    }

    fn manager_live(&mut self) -> Result<bool, String> {
        stack::manager::observe(RuntimePlatform::current(), &mut SystemCommandRunner)
            .map(|job| job.live())
    }

    fn daemon_path(&self) -> Result<PathBuf, String> {
        let platform = RuntimePlatform::current();
        let paths = runtime::session_runtime_paths(
            platform,
            &self.0.root.join("bin"),
            &setup::user_home()?,
        );
        Ok(paths.daemon)
    }

    fn daemon(&mut self) -> Result<Option<Owner>, String> {
        let Ok(list) = self.list() else {
            return Ok(None);
        };
        let daemon = list
            .daemon
            .ok_or("supervisor.list did not report the daemon's identity")?;
        let identity = owner::capture(daemon.pid)?;
        if subc_os::process_start_time_ms(daemon.pid) != Some(daemon.start_time_ms)
            || identity.liveness() != Liveness::Alive
        {
            return Err(format!(
                "daemon identity is unreadable or changed: {}",
                identity.describe()
            ));
        }
        Ok(Some(identity))
    }

    fn module(&mut self, id: &str) -> Result<ModuleReading, String> {
        let list = self.list()?;
        let module = list
            .modules
            .into_iter()
            .find(|module| module.module_id == id)
            .ok_or_else(|| format!("supervisor.list does not list module {id}"))?;
        let generation = module
            .spawn_generation
            .ok_or_else(|| format!("module {id} has no reported spawn generation"))?;
        let image_disagrees = module
            .pending_reload
            .as_ref()
            .is_some_and(|reload| matches!(reload.image, RunningImageAgreement::Mismatch { .. }));
        let run =
            subc_daemon::daemon_config::daemon_run_dir().map_err(|error| error.to_string())?;
        let roster = subc_daemon::read_live_children_roster(&run.join("live-children.json"))
            .map_err(|error| error.to_string())?;
        // The list's `live` bit means serving, not that the OS process exists.
        // Resolve its process using only the strict roster and kernel identity.
        let mut process = None;
        for child in roster
            .into_iter()
            .flat_map(|roster| roster.children)
            .filter(|child| child.module_id == id)
        {
            let unreadable = || {
                format!(
                    "module {id} process identity for pid {} is unreadable",
                    child.pid
                )
            };
            let start = match host::process(child.pid) {
                ProcessReading::Gone => continue,
                ProcessReading::Unreadable => return Err(unreadable()),
                ProcessReading::Alive(start) => start,
            };
            let recorded = child.start_time.ok_or_else(unreadable)?;
            if recorded != start {
                continue;
            }
            let observed = owner::capture(child.pid)?;
            if observed.start_time != recorded
                || child
                    .container
                    .and_then(|container| container.boot_id)
                    .is_some_and(|boot| observed.boot.as_ref() != Some(&boot))
            {
                continue;
            }
            if process.replace(observed).is_some() {
                return Err(format!("module {id} process identity is ambiguous"));
            }
        }
        if process.is_none() && module.live {
            return Err(format!(
                "module {id} process identity is unreadable or ambiguous"
            ));
        }
        Ok(ModuleReading {
            generation,
            process,
            image_disagrees,
        })
    }

    fn has_running_image_proof(&self) -> bool {
        cfg!(target_os = "linux")
    }

    fn image_matches(&self, daemon: &Owner, path: &Path) -> bool {
        if !self.has_running_image_proof() {
            return false;
        }
        let Some(expected) = subc_os::file_identity(path) else {
            return false;
        };
        subc_os::Process::open(daemon.pid)
            .ok()
            .flatten()
            .and_then(|process| process.observe())
            .is_some_and(|observation| {
                observation.start_time == daemon.start_time
                    && observation.executable == Some(expected)
            })
            && daemon.liveness() == Liveness::Alive
    }
}
