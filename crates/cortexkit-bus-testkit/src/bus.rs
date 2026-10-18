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
    /// The HTTP monitoring port nats-server bound, read from its ports file.
    monitor_port: u16,
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
        // nats-server binds every listener on a kernel-chosen port (`:-1`) and
        // reports the result in a ports file. Choosing a free port here and handing
        // the number over would leave a window in which another broker started at
        // the same moment can be given that port too, and the loser exits with
        // "address already in use". The directory is emptied first so a file left by
        // a killed earlier server in a reused fixture tree is never read.
        let ports_dir = dir.join("ports");
        if ports_dir.exists() {
            std::fs::remove_dir_all(&ports_dir)?;
        }
        std::fs::create_dir_all(&ports_dir)?;
        let log = dir.join("server.log");
        let stderr_path = dir.join("server.stderr");
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
            "listen: \"127.0.0.1:-1\"\n\
             max_control_line: 65536\ndebug: true\n\
             http: \"127.0.0.1:-1\"\n\
             ports_file_dir: {ports}\n\
             log_file: {log}\njetstream {{ store_dir: {js} }}\n\
             operator: {operator}\nsystem_account: {system}\n\
             resolver {{ type: full, dir: {jwt}, allow_delete: false, interval: \"2m\" }}\n\
             resolver_preload {{\n{preload}}}\n",
            ports = json!(ports_dir),
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
                // Errors raised before the log file is open (a refused config, a
                // failed listener) only reach stderr.
                .stderr(std::fs::File::create(&stderr_path)?)
                .spawn()?,
        );
        let mut child = child;
        let deadline = Instant::now() + options.startup_timeout;
        let ports = match wait_ports_file(&mut child, &ports_dir, deadline).await {
            Ok(ports) => ports,
            Err(error) => {
                // `try_wait` keeps a status it already collected, so an exit seen
                // inside the wait is reported here.
                return Err(match child.0.try_wait()? {
                    Some(status) => RigError::Invalid(format!(
                        "nats-server exited {status} before writing its ports file: {}",
                        startup_output(&stderr_path, &log)
                    )),
                    None => RigError::Timeout {
                        waiting_for: format!(
                            "nats-server ports file in {} ({error})",
                            ports_dir.display()
                        ),
                        details: startup_output(&stderr_path, &log),
                    },
                });
            }
        };
        let port = ports.client;
        let mut server = Self {
            child,
            _temporary: temporary,
            monitor_port: ports.monitor,
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
        loop {
            if let Some(status) = server.child.0.try_wait()? {
                return Err(RigError::Invalid(format!(
                    "nats-server exited {status}: {}",
                    startup_output(&stderr_path, &server.log)
                )));
            }
            if tokio::time::timeout(Duration::from_millis(500), healthz(server.monitor_port))
                .await
                .unwrap_or(false)
            {
                break;
            }
            if Instant::now() >= deadline {
                return Err(RigError::Timeout {
                    waiting_for: "NATS /healthz".into(),
                    details: startup_output(&stderr_path, &server.log),
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

/// The client and monitoring ports a running nats-server reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BoundPorts {
    client: u16,
    monitor: u16,
}

/// Waits until `child` has written `<executable>_<pid>.ports` under `ports_dir` and
/// returns the ports it names. nats-server writes the file once every listener is
/// bound. The file is written in place, so an unparsable read is a file still
/// being written and is read again; the `Err` is the last reason it was not usable.
async fn wait_ports_file(
    child: &mut Process,
    ports_dir: &std::path::Path,
    deadline: Instant,
) -> Result<BoundPorts, String> {
    let suffix = format!("_{}.ports", child.0.id());
    let mut last = "no ports file written".to_string();
    loop {
        let found = std::fs::read_dir(ports_dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(&suffix))
            });
        if let Some(path) = found {
            match std::fs::read_to_string(&path)
                .map_err(|error| error.to_string())
                .and_then(|text| parse_ports(&text))
            {
                Ok(ports) => return Ok(ports),
                Err(error) => last = format!("{}: {error}", path.display()),
            }
        }
        // A server that already exited will never write the file; the caller
        // reports its exit status.
        if Instant::now() >= deadline || matches!(child.0.try_wait(), Ok(Some(_))) {
            return Err(last);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Parses nats-server's ports file: `{"nats": ["nats://host:port"], "monitoring":
/// ["http://host:port"], ...}`.
fn parse_ports(text: &str) -> Result<BoundPorts, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|error| format!("not JSON yet: {error}"))?;
    let port = |key: &str| -> Result<u16, String> {
        value[key]
            .as_array()
            .and_then(|urls| urls.first())
            .and_then(|url| url.as_str())
            .and_then(|url| url.rsplit(':').next())
            .and_then(|port| port.trim_end_matches('/').parse().ok())
            .ok_or_else(|| format!("no {key} port in {text:?}"))
    };
    Ok(BoundPorts {
        client: port("nats")?,
        monitor: port("monitoring")?,
    })
}

/// The last lines of the server's stderr and log file, for a startup failure.
fn startup_output(stderr: &std::path::Path, log: &std::path::Path) -> String {
    fn tail(path: &std::path::Path) -> String {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(40)..].join("\n")
    }
    format!(
        "stderr tail:\n{}\nserver.log tail:\n{}",
        tail(stderr),
        tail(log)
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Requests `/varz` on `monitor_port` and returns the client port it reports,
    /// confirming that the monitoring and client listeners belong to one server.
    async fn varz_client_port(monitor_port: u16) -> Option<u64> {
        let mut stream = TcpStream::connect((LOOPBACK, monitor_port)).await.ok()?;
        stream
            .write_all(b"GET /varz HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .ok()?;
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.ok()?;
        let text = String::from_utf8_lossy(&buf);
        let (_, body) = text.split_once("\r\n\r\n")?;
        serde_json::from_str::<serde_json::Value>(body).ok()?["port"].as_u64()
    }

    /// Brokers started at the same moment in one process must each get ports nobody
    /// else holds. A kit that picks a free port and releases it before nats-server
    /// binds it can hand one port to two brokers; the loser exits with "address
    /// already in use" and its caller times out on `/healthz`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn eight_concurrent_brokers_bind_distinct_ports_and_all_answer_healthz() {
        if let Err(error) = crate::nats_server_bin() {
            crate::skip_or_fail(error);
            return;
        }
        let started =
            futures_util::future::join_all((0..8).map(|_| BusServer::start(BusOptions::default())))
                .await;
        let servers: Vec<BusServer> = started
            .into_iter()
            .enumerate()
            .map(|(index, result)| {
                result.unwrap_or_else(|error| panic!("broker {index} did not start: {error}"))
            })
            .collect();
        let mut seen = BTreeSet::new();
        for (index, server) in servers.iter().enumerate() {
            for (kind, port) in [("client", server.port), ("monitor", server.monitor_port)] {
                assert!(
                    seen.insert(port),
                    "broker {index}'s {kind} port {port} is already used by another broker"
                );
            }
        }
        assert_eq!(seen.len(), 16, "8 client and 8 monitor ports, all distinct");
        for (index, server) in servers.iter().enumerate() {
            assert!(
                healthz(server.monitor_port).await,
                "broker {index} /healthz on {} did not answer ok: {}",
                server.monitor_port,
                server.log_text()
            );
            assert_eq!(
                varz_client_port(server.monitor_port).await,
                Some(u64::from(server.port)),
                "broker {index}'s monitor and client ports belong to different servers"
            );
            // The kit asked nats-server to choose; it never chose a number itself.
            let conf = std::fs::read_to_string(server.dir.join("server.conf")).unwrap();
            for line in ["listen: \"127.0.0.1:-1\"\n", "http: \"127.0.0.1:-1\"\n"] {
                assert!(
                    conf.contains(line),
                    "broker {index} conf lacks {line:?}: {conf}"
                );
            }
        }
    }

    #[test]
    fn ports_file_parsing_reads_both_ports_and_rejects_partial_writes() {
        assert_eq!(
            parse_ports(
                r#"{"nats":["nats://127.0.0.1:41441"],"monitoring":["http://127.0.0.1:36891"]}"#
            ),
            Ok(BoundPorts {
                client: 41441,
                monitor: 36891
            })
        );
        assert!(parse_ports(r#"{"nats":["nats://127.0.0.1:41441"],"mon"#).is_err());
        assert!(parse_ports(r#"{"nats":["nats://127.0.0.1:41441"]}"#).is_err());
        assert!(parse_ports("").is_err());
    }
}
