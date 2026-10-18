use crate::{process::Process, BusServer, Module, RigError};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use subc_control::{ClientControlRequest, ClientControlResponse};
use subc_protocol::{Flags, Frame, FrameType, Priority};
use subc_transport::{authenticate_client, connection_file, read_frame, write_frame};
use tokio::{io::AsyncWriteExt, net::TcpStream, task::JoinHandle};

pub struct DaemonOptions {
    pub modules: Vec<Module>,
    /// A 32-character lowercase hex id. None suppresses the fixture file.
    /// The daemon version under test may itself mint an id when none exists.
    pub machine_id: Option<String>,
    pub startup_timeout: Duration,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        Self {
            modules: vec![],
            machine_id: Some(new_machine_id()),
            startup_timeout: Duration::from_secs(30),
        }
    }
}

pub fn new_machine_id() -> String {
    let key = nkeys::KeyPair::new_user();
    crate::signer::hex(&Sha256::digest(key.public_key().as_bytes())[..16])
}

/// Write the identity in the data home the binary daemon uses, not the caller's.
pub fn write_machine_id(data_home: &Path, machine_id: &str) -> Result<PathBuf, RigError> {
    if machine_id.len() != 32
        || !machine_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(RigError::Invalid(
            "machine id must be 32 lowercase hex characters".into(),
        ));
    }
    let path = data_home.join("cortexkit/machine-id");
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, format!("{machine_id}\n"))?;
    Ok(path)
}

/// A binary daemon and a throwaway, in-client-process claustrum signer.
/// The broker must outlive this value. No operator vault or installed daemon is used.
pub struct PrivateDaemon {
    child: Option<Process>,
    signer: Option<JoinHandle<Result<(), subc_client_rs::SubcModuleError>>>,
    root: tempfile::TempDir,
    pub connection_file: PathBuf,
    pub config_file: PathBuf,
    pub data_home: PathBuf,
    pub runtime_dir: PathBuf,
    pub config_home: PathBuf,
    log: PathBuf,
}

impl PrivateDaemon {
    pub async fn start(bus: &BusServer, options: DaemonOptions) -> Result<Self, RigError> {
        // Resolve every required binary before creating state or starting a child.
        let subc = crate::ck_subc_binary()?;
        let ck_bus = crate::binaries::ck_bus_source()?;
        let root = tempfile::tempdir()?;
        let data_home = root.path().join("data");
        let runtime_dir = root.path().join("run");
        let config_home = root.path().join("config");
        for dir in [&data_home, &runtime_dir, &config_home.join("cortexkit")] {
            std::fs::create_dir_all(dir)?;
        }
        if let Some(machine_id) = &options.machine_id {
            write_machine_id(&data_home, machine_id)?;
        }
        let config_file = config_home.join("cortexkit/subc.jsonc");
        std::fs::write(
            &config_file,
            crate::subc_config(&ck_bus, &bus.ckbus_env(), &options.modules)?,
        )?;
        let connection_file = runtime_dir.join("subc-connection.json");
        let log = root.path().join("daemon.log");
        let output = std::fs::File::create(&log)?;
        // Set homes on the child, never the test process. The daemon's orphan sweep
        // must not inspect or signal processes recorded in the operator's data home.
        let child = Process(
            Command::new(subc)
                .env("XDG_DATA_HOME", &data_home)
                .env("XDG_RUNTIME_DIR", &runtime_dir)
                .env("XDG_CONFIG_HOME", &config_home)
                .env("SUBC_CGROUP_PLACEMENT", "disabled")
                .env("SUBC_PORT", "0")
                .stdin(Stdio::null())
                .stdout(output.try_clone()?)
                .stderr(output)
                .spawn()?,
        );
        let mut daemon = Self {
            child: Some(child),
            signer: None,
            root,
            connection_file,
            config_file,
            data_home,
            runtime_dir,
            config_home,
            log,
        };
        let deadline = Instant::now() + options.startup_timeout;
        daemon.wait_connection(deadline).await?;
        let signer = bus.trust.signer.clone();
        let connection = daemon.connection_file.clone();
        daemon.signer = Some(tokio::spawn(async move {
            subc_client_rs::serve_with(&connection, signer.manifest(), signer).await
        }));
        loop {
            daemon.check_child()?;
            let response = tokio::time::timeout_at(
                tokio::time::Instant::from_std(deadline),
                daemon.control(ClientControlRequest::CatalogList {
                    module_id: Some("ckbus".into()),
                }),
            )
            .await;
            if let Ok(Ok(ClientControlResponse::CatalogList { modules, .. })) = response {
                if modules.iter().any(|module| module.module_id == "ckbus") {
                    return Ok(daemon);
                }
            }
            if Instant::now() >= deadline {
                return Err(daemon.timeout("ckbus catalog registration"));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn wait_connection(&mut self, deadline: Instant) -> Result<(), RigError> {
        loop {
            self.check_child()?;
            if connection_file::read(&self.connection_file).is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(self.timeout("private connection file"));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    fn check_child(&mut self) -> Result<(), RigError> {
        if let Some(status) = self.child.as_mut().unwrap().0.try_wait()? {
            return Err(RigError::Invalid(format!(
                "ck-subc exited {status}: {}",
                self.log_text()
            )));
        }
        Ok(())
    }
    fn timeout(&self, waiting_for: &str) -> RigError {
        RigError::Timeout {
            waiting_for: waiting_for.into(),
            details: format!(
                "expected {}; {}",
                self.connection_file.display(),
                self.log_text()
            ),
        }
    }
    pub fn root(&self) -> &Path {
        self.root.path()
    }
    pub fn pid(&self) -> u32 {
        self.child.as_ref().unwrap().0.id()
    }
    pub fn log_text(&self) -> String {
        let mut text = std::fs::read_to_string(&self.log).unwrap_or_default();
        let mut directories = vec![self.data_home.join("cortexkit/run/logs")];
        while let Some(directory) = directories.pop() {
            let Ok(entries) = std::fs::read_dir(directory) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    directories.push(entry.path());
                } else {
                    text.push_str(&format!(
                        "\n{}:\n{}",
                        entry.path().display(),
                        std::fs::read_to_string(entry.path()).unwrap_or_default()
                    ));
                }
            }
        }
        text
    }

    pub async fn control(
        &self,
        request: ClientControlRequest,
    ) -> Result<ClientControlResponse, RigError> {
        tokio::time::timeout(
            Duration::from_secs(3),
            control(&self.connection_file, request),
        )
        .await
        .map_err(|_| self.timeout("daemon control response"))?
    }
}

impl Drop for PrivateDaemon {
    fn drop(&mut self) {
        drop(self.child.take());
        if let Some(signer) = self.signer.take() {
            signer.abort();
        }
    }
}

async fn control(
    path: &Path,
    request: ClientControlRequest,
) -> Result<ClientControlResponse, RigError> {
    let connection =
        connection_file::read(path).map_err(|error| RigError::Invalid(error.to_string()))?;
    let endpoint = connection
        .endpoints
        .first()
        .ok_or_else(|| RigError::Invalid("no daemon endpoint".into()))?;
    let mut stream = TcpStream::connect((endpoint.host.as_str(), endpoint.port)).await?;
    authenticate_client(&mut stream, &connection, Duration::from_secs(2))
        .await
        .map_err(|error| RigError::Invalid(error.to_string()))?;
    let frame = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        1,
        serde_json::to_vec(&request).map_err(|error| RigError::Invalid(error.to_string()))?,
    )
    .map_err(|error| RigError::Invalid(error.to_string()))?;
    write_frame(&mut stream, &frame)
        .await
        .map_err(|error| RigError::Invalid(error.to_string()))?;
    stream.flush().await?;
    let response = read_frame(&mut stream)
        .await
        .map_err(|error| RigError::Invalid(error.to_string()))?
        .ok_or_else(|| RigError::Invalid("daemon closed control connection".into()))?;
    if response.header.ty != FrameType::Response {
        return Err(RigError::Invalid(format!(
            "control refused: {}",
            String::from_utf8_lossy(&response.body)
        )));
    }
    serde_json::from_slice(&response.body).map_err(|error| RigError::Invalid(error.to_string()))
}
