use std::{
    net::IpAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use subc_client_rs::{ConsumerOptions, SubcConsumer, SupervisorListResponse};
use subc_control::RunningImageAgreement;
use subc_daemon::{read_frame, write_frame, Frame};
use subc_protocol::{Flags, FrameType, Priority};
use subc_transport::{authenticate_client, connection_file};

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
            let mut stream = match tokio::time::timeout(QUERY, async {
                let found = connection_file::discover(self.0.connection.as_deref()).ok()?;
                let endpoint = found.info.endpoints.first()?;
                let ip = endpoint.host.parse::<IpAddr>().ok()?;
                let mut stream = tokio::net::TcpStream::connect((ip, endpoint.port))
                    .await
                    .ok()?;
                authenticate_client(&mut stream, &found.info, QUERY)
                    .await
                    .ok()?;
                Some(stream)
            })
            .await
            .ok()
            .flatten()
            {
                Some(stream) => stream,
                None => return Ok(None),
            };
            let result = tokio::time::timeout(QUERY, async {
                let request = Frame::build(
                    FrameType::Request,
                    Flags::new(false, Priority::Interactive, false),
                    0,
                    0,
                    1,
                    br#"{"op":"config.status"}"#.to_vec(),
                )
                .map_err(|error| error.to_string())?;
                write_frame(&mut stream, &request)
                    .await
                    .map_err(|error| error.to_string())?;
                loop {
                    let frame = read_frame(&mut stream)
                        .await
                        .map_err(|error| error.to_string())?
                        .ok_or("daemon closed the connection")?;
                    if frame.header.channel != 0 || frame.header.corr != 1 {
                        continue;
                    }
                    if frame.header.ty != FrameType::Response {
                        return Err("daemon did not return config.status".into());
                    }
                    let response: serde_json::Value =
                        serde_json::from_slice(&frame.body).map_err(|error| error.to_string())?;
                    if response.get("op").and_then(serde_json::Value::as_str)
                        != Some("config.status")
                    {
                        return Err("daemon returned an unexpected config.status response".into());
                    }
                    let status = response
                        .get("status")
                        .ok_or("daemon did not report configuration protection")?;
                    return match status.get("state").and_then(serde_json::Value::as_str) {
                        Some("stamped") => Ok(Some(true)),
                        Some("unstamped")
                            if status
                                .get("anchor_pending")
                                .and_then(serde_json::Value::as_bool)
                                != Some(true) =>
                        {
                            Ok(Some(false))
                        }
                        state => Err(format!(
                            "configuration protection could not be confirmed: {state:?}"
                        )),
                    };
                }
            })
            .await;
            match result {
                Ok(value) => value,
                Err(_) => Ok(None),
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
