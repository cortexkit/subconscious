//! Stock NATS server, JWT and leaf-cut fixtures for the federation rigs.

use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::Engine as _;
use nkeys::KeyPair;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::{Child, Command},
    sync::Mutex,
    task::JoinHandle,
};

/// The tests share the machine's loopback ports and CPU; running them one at a time
/// keeps their `--nocapture` reports readable and their timings meaningful.
pub(super) static RIG_LOCK: Mutex<()> = Mutex::const_new(());

pub(super) fn note(test: u8, msg: impl AsRef<str>) {
    println!("[rig {test}] {}", msg.as_ref());
}

pub(super) fn nats_server_bin() -> PathBuf {
    if let Some(path) = std::env::var_os("NATS_SERVER_BIN") {
        let path = PathBuf::from(path);
        assert!(
            path.is_file(),
            "NATS RIG NOT RUN: NATS_SERVER_BIN={} is not a file. This rig measures the real \
             server and refuses to pass without it.",
            path.display()
        );
        return path;
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("nats-server");
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!(
        "NATS RIG NOT RUN: no `nats-server` binary (set NATS_SERVER_BIN or put it on PATH). \
         This rig measures the real server and refuses to pass without it."
    );
}

// ---------------------------------------------------------------------------------
// Keys, JWTs and creds
// ---------------------------------------------------------------------------------

pub(super) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs() as i64
}

pub(super) fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Encodes and signs a nats-io/jwt v2 token. The server checks the header, the
/// issuer's signature over `header.claims`, and the claim contents; `jti` only has to
/// be unique, so a SHA-256 of the claims stands in for the library's own hash.
pub(super) fn encode_jwt(
    signer: &KeyPair,
    subject: &str,
    name: &str,
    iat: i64,
    nats: Value,
) -> String {
    let header = json!({"typ": "JWT", "alg": "ed25519-nkey"});
    let mut claims = json!({
        "jti": "",
        "iat": iat,
        "iss": signer.public_key(),
        "name": name,
        "sub": subject,
        "nats": nats,
    });
    let digest = Sha256::digest(claims.to_string().as_bytes());
    claims["jti"] = Value::String(digest.iter().map(|b| format!("{b:02X}")).collect());
    let signing_input = format!(
        "{}.{}",
        b64url(header.to_string().as_bytes()),
        b64url(claims.to_string().as_bytes())
    );
    let signature = signer.sign(signing_input.as_bytes()).expect("sign jwt");
    format!("{signing_input}.{}", b64url(&signature))
}

pub(super) struct User {
    kp: KeyPair,
    pub(super) creds: String,
}

impl User {
    pub(super) fn public_key(&self) -> String {
        self.kp.public_key()
    }
}

pub(super) struct Account {
    name: String,
    pub(super) kp: KeyPair,
    jetstream: bool,
    pub(super) iat: i64,
    pub(super) revocations: BTreeMap<String, i64>,
    pub(super) imports: Vec<Value>,
    pub(super) exports: Vec<Value>,
}

impl Account {
    pub(super) fn new(name: &str, jetstream: bool) -> Self {
        Self {
            name: name.to_string(),
            kp: KeyPair::new_account(),
            jetstream,
            // Back-dated so a later update (revocation) always carries a newer iat.
            iat: unix_now() - 120,
            revocations: BTreeMap::new(),
            imports: Vec::new(),
            exports: Vec::new(),
        }
    }

    pub(super) fn public_key(&self) -> String {
        self.kp.public_key()
    }

    pub(super) fn jwt(&self, operator: &KeyPair) -> String {
        // Every limit is explicit: a missing limit decodes as 0, which the server
        // enforces as "none allowed" (no connections, no leafs, no JetStream).
        let mut limits = json!({
            "subs": -1, "data": -1, "payload": -1, "imports": -1, "exports": -1,
            "wildcards": true, "conn": -1, "leaf": -1,
        });
        if self.jetstream {
            limits["mem_storage"] = json!(-1);
            limits["disk_storage"] = json!(-1);
            limits["streams"] = json!(-1);
            limits["consumer"] = json!(-1);
        }
        let mut nats = json!({
            "type": "account",
            "version": 2,
            "limits": limits,
            "default_permissions": {"pub": {}, "sub": {}},
        });
        if !self.revocations.is_empty() {
            nats["revocations"] = json!(self.revocations);
        }
        if !self.imports.is_empty() {
            nats["imports"] = json!(self.imports);
        }
        if !self.exports.is_empty() {
            nats["exports"] = json!(self.exports);
        }
        encode_jwt(operator, &self.public_key(), &self.name, self.iat, nats)
    }

    /// Issues a user in this account with no permission restrictions, back-dated one
    /// minute so a revocation stamped "now" covers it.
    pub(super) fn user(&self, name: &str) -> User {
        self.user_with_permission_claims(name, json!({}), json!({}))
    }

    pub(super) fn user_with_permissions(
        &self,
        name: &str,
        publish: &[String],
        subscribe: &[String],
    ) -> User {
        self.user_with_permission_claims(
            name,
            json!({"allow": publish}),
            json!({"allow": subscribe}),
        )
    }

    pub(super) fn user_with_permission_claims(
        &self,
        name: &str,
        publish: Value,
        subscribe: Value,
    ) -> User {
        let kp = KeyPair::new_user();
        let nats = json!({
            "type": "user", "version": 2, "pub": publish, "sub": subscribe,
            "subs": -1, "data": -1, "payload": -1,
        });
        let jwt = encode_jwt(&self.kp, &kp.public_key(), name, unix_now() - 60, nats);
        let seed = kp.seed().expect("user seed");
        let creds = format!(
            "-----BEGIN NATS USER JWT-----\n{jwt}\n------END NATS USER JWT------\n\n\
             -----BEGIN USER NKEY SEED-----\n{seed}\n------END USER NKEY SEED------\n"
        );
        User { kp, creds }
    }
}

pub(super) struct Operator {
    name: String,
    pub(super) kp: KeyPair,
    sys: Account,
    pub(super) sys_user: User,
}

impl Operator {
    pub(super) fn new(name: &str) -> Self {
        let sys = Account::new(&format!("{name}-SYS"), false);
        let sys_user = sys.user(&format!("{name}-sys-user"));
        Self {
            name: name.to_string(),
            kp: KeyPair::new_operator(),
            sys,
            sys_user,
        }
    }

    pub(super) fn jwt(&self) -> String {
        let nats = json!({
            "type": "operator", "version": 2,
            "system_account": self.sys.public_key(),
        });
        encode_jwt(
            &self.kp,
            &self.kp.public_key(),
            &self.name,
            unix_now() - 120,
            nats,
        )
    }
}

// ---------------------------------------------------------------------------------
// Servers, monitoring and the split proxy
// ---------------------------------------------------------------------------------

/// Returns the client and monitoring ports bound by the nats-server `child`, read
/// from the `<executable>_<pid>.ports` file it writes under `ports_dir` once every
/// listener is up. Servers listen on kernel-chosen ports (`:-1`): choosing a free port here
/// and handing the number over would let two servers started together be given
/// the same port, and the loser exits with "address already in use".
pub(super) async fn bound_ports(
    child: &mut Child,
    ports_dir: &Path,
    stderr: &Path,
    log: &Path,
    limit: Duration,
) -> (u16, u16) {
    let pid = child.id().expect("a running nats-server has a pid");
    let suffix = format!("_{pid}.ports");
    let deadline = Instant::now() + limit;
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
        // The file is written in place; an unparsable read is a write in progress.
        if let Some(ports) = found
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| Some((url_port(&value["nats"])?, url_port(&value["monitoring"])?)))
        {
            return ports;
        }
        let exited = child.try_wait().ok().flatten();
        if exited.is_some() || Instant::now() >= deadline {
            let tail = |path: &Path| {
                let text = std::fs::read_to_string(path).unwrap_or_default();
                let lines: Vec<&str> = text.lines().collect();
                lines[lines.len().saturating_sub(40)..].join("\n")
            };
            panic!(
                "nats-server ports file never appeared in {} (exit: {exited:?})\n\
                 stderr tail:\n{}\nserver.log tail:\n{}",
                ports_dir.display(),
                tail(stderr),
                tail(log)
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The port of the first URL in a ports-file list such as `["nats://127.0.0.1:4222"]`.
pub(super) fn url_port(urls: &Value) -> Option<u16> {
    urls.get(0)?.as_str()?.rsplit(':').next()?.parse().ok()
}

pub(super) async fn wait_for<F, Fut>(what: &str, limit: Duration, mut probe: F) -> Duration
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let start = Instant::now();
    loop {
        if probe().await {
            return start.elapsed();
        }
        assert!(
            start.elapsed() < limit,
            "timed out after {limit:?} waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub(super) async fn http_get(port: u16, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
    let request = format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n");
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    text.split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
}

pub(super) struct Server {
    pub(super) name: String,
    bin: PathBuf,
    pub(super) conf: PathBuf,
    log: PathBuf,
    /// Bound by the running server; zero until `start` reads its ports file.
    client_port: u16,
    pub(super) http_port: u16,
    child: Option<Child>,
}

/// How a server resolves account JWTs.
pub(super) enum Resolver {
    /// Preloaded, read-only: enough for a box server whose accounts never change here.
    Memory,
    /// The directory resolver, which accepts signed account updates over `$SYS`.
    Full,
}

impl Server {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn configure(
        bin: &Path,
        root: &Path,
        name: &str,
        domain: &str,
        operator: &Operator,
        accounts: &[&Account],
        resolver: Resolver,
        extra: &str,
    ) -> Server {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("server dir");
        let operator_path = dir.join("operator.jwt");
        std::fs::write(&operator_path, operator.jwt()).expect("operator jwt");
        let log = dir.join("server.log");
        let mut preload = format!(
            "  {}: \"{}\"\n",
            operator.sys.public_key(),
            operator.sys.jwt(&operator.kp)
        );
        for account in accounts {
            preload.push_str(&format!(
                "  {}: \"{}\"\n",
                account.public_key(),
                account.jwt(&operator.kp)
            ));
        }
        let resolver = match resolver {
            Resolver::Memory => "resolver: MEMORY".to_string(),
            Resolver::Full => format!(
                "resolver {{ type: full, dir: \"{}\", allow_delete: false, interval: \"2m\" }}",
                dir.join("jwt").display()
            ),
        };
        let conf = format!(
            "server_name: \"{name}\"\n\
             listen: \"127.0.0.1:-1\"\n\
             http: \"127.0.0.1:-1\"\n\
             ports_file_dir: \"{ports}\"\n\
             log_file: \"{log}\"\n\
             logtime: true\n\
             jetstream {{ store_dir: \"{js}\", domain: \"{domain}\" }}\n\
             operator: \"{op}\"\n\
             system_account: \"{sys}\"\n\
             {resolver}\n\
             resolver_preload {{\n{preload}}}\n\
             {extra}\n",
            ports = dir.join("ports").display(),
            log = log.display(),
            js = dir.join("js").display(),
            op = operator_path.display(),
            sys = operator.sys.public_key(),
        );
        let conf_path = dir.join("server.conf");
        std::fs::write(&conf_path, conf).expect("server conf");
        Server {
            name: name.to_string(),
            bin: bin.to_path_buf(),
            conf: conf_path,
            log,
            client_port: 0,
            http_port: 0,
            child: None,
        }
    }

    pub(super) fn url(&self) -> String {
        format!("nats://127.0.0.1:{}", self.client_port)
    }

    /// Starts the server and waits for `/healthz`. A restarted server binds new
    /// ports, so `url()` and the monitoring port are only valid after this returns.
    pub(super) async fn start(&mut self) {
        let dir = self.conf.parent().unwrap().to_path_buf();
        // A file left by a killed earlier run must never be read as this run's.
        let ports_dir = dir.join("ports");
        let _ = std::fs::remove_dir_all(&ports_dir);
        std::fs::create_dir_all(&ports_dir).expect("ports dir");
        let stderr = dir.join("server.stderr");
        let mut child = Command::new(&self.bin)
            .arg("-c")
            .arg(&self.conf)
            .env("XDG_DATA_HOME", self.conf.parent().unwrap().join("data"))
            .env("XDG_RUNTIME_DIR", self.conf.parent().unwrap().join("run"))
            .env(
                "XDG_CONFIG_HOME",
                self.conf.parent().unwrap().join("config"),
            )
            .stdout(Stdio::null())
            // Errors raised before the log file is open only reach stderr.
            .stderr(std::fs::File::create(&stderr).expect("server stderr file"))
            .kill_on_drop(true)
            .spawn()
            .expect("spawn nats-server");
        (self.client_port, self.http_port) = bound_ports(
            &mut child,
            &ports_dir,
            &stderr,
            &self.log,
            Duration::from_secs(20),
        )
        .await;
        self.child = Some(child);
        let port = self.http_port;
        let what = format!("{} /healthz ok", self.name);
        wait_for(&what, Duration::from_secs(20), move || async move {
            http_get(port, "/healthz")
                .await
                .is_some_and(|body| body.contains("\"ok\""))
        })
        .await;
    }

    /// Stops the server the way a supervisor would: SIGTERM, then SIGKILL after 10s.
    pub(super) async fn stop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if let Some(pid) = child.id() {
            let _ = Command::new("kill")
                .arg("-TERM")
                .arg(pid.to_string())
                .status()
                .await;
        }
        if tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
    }

    pub(super) fn log_lines(&self, needles: &[&str]) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter(|line| needles.iter().any(|needle| line.contains(needle)))
            .map(str::to_string)
            .collect()
    }

    pub(super) fn print_log(&self, test: u8, needles: &[&str]) {
        for line in self.log_lines(needles) {
            note(test, format!("  {} log: {line}", self.name));
        }
    }

    pub(super) async fn leafs(&self) -> Vec<(String, String)> {
        leafs_at(self.http_port).await
    }

    pub(super) async fn wait_leaf_names(&self, want: &[&str], limit: Duration) -> Duration {
        let mut want: Vec<String> = want.iter().map(|s| s.to_string()).collect();
        want.sort();
        let port = self.http_port;
        let what = format!("{} leafs == {want:?}", self.name);
        wait_for(&what, limit, || {
            let want = want.clone();
            async move {
                let mut names: Vec<String> =
                    leafs_at(port).await.into_iter().map(|(n, _)| n).collect();
                names.sort();
                names == want
            }
        })
        .await
    }
}

/// `(remote server name, bound account)` for every leaf connection, from `/leafz`.
pub(super) async fn leafs_at(http_port: u16) -> Vec<(String, String)> {
    let Some(body) = http_get(http_port, "/leafz").await else {
        return Vec::new();
    };
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    parsed["leafs"]
        .as_array()
        .map(|leafs| {
            leafs
                .iter()
                .map(|leaf| {
                    (
                        leaf["name"].as_str().unwrap_or_default().to_string(),
                        leaf["account"].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A loopback TCP relay standing in for the network between a box and the hub.
/// `cut` drops every relayed connection and refuses new ones until `restore`, which
/// is what the leaf sees when the link breaks; neither server is touched.
pub(super) struct Proxy {
    pub(super) port: u16,
    cut: Arc<AtomicBool>,
    conns: Arc<std::sync::Mutex<Vec<JoinHandle<()>>>>,
    accept: JoinHandle<()>,
}

impl Proxy {
    pub(super) async fn start(upstream: u16) -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("proxy bind");
        let port = listener.local_addr().expect("proxy addr").port();
        let cut = Arc::new(AtomicBool::new(false));
        let conns = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (cut_flag, conn_list) = (cut.clone(), conns.clone());
        let accept = tokio::spawn(async move {
            loop {
                let Ok((mut inbound, _)) = listener.accept().await else {
                    continue;
                };
                if cut_flag.load(Ordering::SeqCst) {
                    drop(inbound);
                    continue;
                }
                let relay = tokio::spawn(async move {
                    if let Ok(mut outbound) = TcpStream::connect(("127.0.0.1", upstream)).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                });
                conn_list.lock().expect("proxy conns").push(relay);
            }
        });
        Proxy {
            port,
            cut,
            conns,
            accept,
        }
    }

    pub(super) fn cut(&self) {
        self.cut.store(true, Ordering::SeqCst);
        for relay in self.conns.lock().expect("proxy conns").drain(..) {
            relay.abort();
        }
    }

    pub(super) fn restore(&self) {
        self.cut.store(false, Ordering::SeqCst);
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.accept.abort();
        self.cut();
    }
}

pub(super) async fn connect(url: &str, user: &User) -> async_nats::Client {
    async_nats::ConnectOptions::with_credentials(&user.creds)
        .expect("parse creds")
        .connect(url)
        .await
        .unwrap_or_else(|err| panic!("connect {url}: {err}"))
}
