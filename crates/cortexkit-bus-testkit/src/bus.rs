use crate::{
    process::Process,
    trust::{unix_now, TrustChain},
    RigError,
};
use serde_json::json;
use std::{
    collections::BTreeSet,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

pub const LOOPBACK: &str = "127.0.0.1";

/// An additional account for tests that exercise user JWTs without ck-bus bootstrap.
pub struct PreloadedAccount {
    pub public_key: String,
    pub signing_keys: Vec<String>,
}

pub struct BusOptions {
    /// Optional test-build path; otherwise use CK_NATS_SERVER_BIN, then PATH.
    pub binary: Option<PathBuf>,
    /// Optional externally owned fixture directory; otherwise a temporary tree.
    pub directory: Option<PathBuf>,
    pub trust: Option<TrustChain>,
    pub preload: Vec<PreloadedAccount>,
    pub startup_timeout: Duration,
}

impl Default for BusOptions {
    fn default() -> Self {
        Self {
            binary: None,
            directory: None,
            trust: None,
            preload: vec![],
            startup_timeout: Duration::from_secs(20),
        }
    }
}

pub struct BusServer {
    // Drop processes before their files.
    child: Process,
    _temporary: Option<tempfile::TempDir>,
    pub dir: PathBuf,
    pub url: String,
    pub port: u16,
    pub listen_host: String,
    pub log: PathBuf,
    pub operator_jwt: PathBuf,
    pub system_account: String,
    pub trust: TrustChain,
    pub version: String,
}

impl BusServer {
    pub async fn start(options: BusOptions) -> Result<Self, RigError> {
        let (bin, version) = match options.binary {
            Some(path) => crate::binaries::checked_nats(path)?,
            None => crate::nats_server_bin()?,
        };
        let temporary = if options.directory.is_none() {
            Some(tempfile::tempdir()?)
        } else {
            None
        };
        let dir = options
            .directory
            .unwrap_or_else(|| temporary.as_ref().unwrap().path().to_path_buf());
        std::fs::create_dir_all(dir.join("jwt"))?;
        let dir = dir.canonicalize()?;
        let trust = options.trust.unwrap_or_else(TrustChain::generate);
        let operator_jwt = dir.join("operator.jwt");
        std::fs::write(&operator_jwt, trust.operator_jwt())?;
        let port = free_port()?;
        let mut http_port = free_port()?;
        while http_port == port {
            http_port = free_port()?;
        }
        let log = dir.join("server.log");
        let mut preload = format!(
            "{}: {}\n",
            trust.system_account,
            json!(trust.system_account_jwt())
        );
        for account in options.preload {
            let jwt = trust.account_jwt(&account.public_key, &account.signing_keys);
            preload.push_str(&format!("{}: {}\n", account.public_key, json!(jwt)));
        }
        // A participant's JWT can exceed NATS's default 4 KiB CONNECT limit.
        // JSON string quoting also escapes fixture paths containing quotes or slashes.
        let conf = format!(
            "listen: \"127.0.0.1:{port}\"\n\
             max_control_line: 65536\ndebug: true\n\
             http: \"127.0.0.1:{http_port}\"\n\
             log_file: {log}\njetstream {{ store_dir: {js} }}\n\
             operator: {operator}\nsystem_account: {system}\n\
             resolver {{ type: full, dir: {jwt}, allow_delete: false, interval: \"2m\" }}\n\
             resolver_preload {{\n{preload}}}\n",
            log = json!(log),
            js = json!(dir.join("js")),
            operator = json!(operator_jwt),
            system = json!(trust.system_account),
            jwt = json!(dir.join("jwt")),
        );
        let conf_path = dir.join("server.conf");
        std::fs::write(&conf_path, conf)?;
        let child = Process(
            Command::new(bin)
                .arg("-c")
                .arg(conf_path)
                .env("XDG_DATA_HOME", dir.join("data"))
                .env("XDG_RUNTIME_DIR", dir.join("run"))
                .env("XDG_CONFIG_HOME", dir.join("config"))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let mut server = Self {
            child,
            _temporary: temporary,
            dir,
            url: format!("nats://127.0.0.1:{port}"),
            port,
            listen_host: LOOPBACK.to_string(),
            log,
            operator_jwt,
            system_account: trust.system_account.clone(),
            trust,
            version,
        };
        let deadline = Instant::now() + options.startup_timeout;
        loop {
            if let Some(status) = server.child.0.try_wait()? {
                return Err(RigError::Invalid(format!(
                    "nats-server exited {status}: {}",
                    server.log_text()
                )));
            }
            if tokio::time::timeout(Duration::from_millis(500), healthz(http_port))
                .await
                .unwrap_or(false)
            {
                break;
            }
            if Instant::now() >= deadline {
                return Err(RigError::Timeout {
                    waiting_for: "NATS /healthz".into(),
                    details: server.log_text(),
                });
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(server)
    }

    pub fn pid(&self) -> u32 {
        self.child.0.id()
    }

    /// Bootstrap inputs for the supervised ck-bus module.
    pub fn ckbus_env(&self) -> Vec<(String, String)> {
        vec![
            ("CKBUS_NATS_URL".into(), self.url.clone()),
            (
                "CKBUS_OPERATOR_JWT".into(),
                self.operator_jwt.display().to_string(),
            ),
            ("CKBUS_SYSTEM_ACCOUNT".into(), self.system_account.clone()),
        ]
    }

    pub fn stored_accounts(&self) -> BTreeSet<String> {
        std::fs::read_dir(self.dir.join("jwt"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.strip_suffix(".jwt"))
                    .map(str::to_string)
            })
            .collect()
    }
    pub fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
    pub async fn stop(self) {
        drop(self);
    }
}

fn free_port() -> Result<u16, RigError> {
    Ok(std::net::TcpListener::bind((LOOPBACK, 0))?
        .local_addr()?
        .port())
}

async fn healthz(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect((LOOPBACK, port)).await else {
        return false;
    };
    if stream
        .write_all(b"GET /healthz HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .is_err()
    {
        return false;
    }
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.is_ok() && String::from_utf8_lossy(&buf).contains("\"ok\"")
}

impl TrustChain {
    pub(crate) fn account_jwt(&self, public_key: &str, signing_keys: &[String]) -> String {
        crate::trust::encode_jwt(
            &self.operator_root,
            public_key,
            "fixture-account",
            unix_now() - 120,
            json!({"type": "account", "version": 2,
                "limits": {"subs": -1, "data": -1, "payload": -1, "imports": -1, "exports": -1,
                    "wildcards": true, "conn": -1, "leaf": -1},
                "signing_keys": signing_keys, "default_permissions": {"pub": {}, "sub": {}}}),
        )
    }
}
