//! Rig for the stock `nats-server` behaviours that `docs/designs/nats-federation.md`
//! depends on. The measured results are written up in
//! `docs/designs/nats-federation-rig-results.md`.
//!
//! Every test is `#[ignore]`d because it needs a real `nats-server` binary, found
//! through `NATS_SERVER_BIN` or `PATH`. A missing binary FAILS the test with a clear
//! message rather than passing: an ignored rig runs only when someone asked for the
//! measurement, and a silent pass would report a measurement that never happened.
//!
//! Run: `cargo test -p ck-bus --test nats_federation_rig -- --ignored --nocapture`
//!
//! Topology per test, in its own temp directory, all on loopback with free ports:
//!
//! ```text
//!  box A server (own operator)          hub server (hub operator)            box B server (own operator)
//!  accounts LOCAL_A, FED_A  --leaf-->   account U (one per user)   <--leaf-- accounts LOCAL_B, FED_B
//!  JetStream domain "a"      (proxy)    JetStream domain "hub"      (proxy)  JetStream domain "b"
//! ```
//!
//! Each leaf remote binds only the box's federation account and authenticates with a
//! user JWT issued by the hub account U. Every leaf link runs through an in-test TCP
//! proxy so a network split can be cut and restored without stopping either server.
//!
//! Keys are generated in-process with the `nkeys` crate. `nsc` is not installed, so
//! operator, account and user JWTs are assembled here in the nats-io/jwt v2 format
//! (header `{"typ":"JWT","alg":"ed25519-nkey"}`, claims, ed25519 signature by the
//! issuer's nkey). All servers run in operator (JWT) mode; the hub uses the `full`
//! (directory) account resolver so account updates can be pushed over `$SYS`.

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

use async_nats::jetstream::{self, stream};
use base64::Engine as _;
use futures_util::StreamExt;
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
static RIG_LOCK: Mutex<()> = Mutex::const_new(());

fn note(test: u8, msg: impl AsRef<str>) {
    println!("[rig {test}] {}", msg.as_ref());
}

fn nats_server_bin() -> PathBuf {
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

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs() as i64
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Encodes and signs a nats-io/jwt v2 token. The server checks the header, the
/// issuer's signature over `header.claims`, and the claim contents; `jti` only has to
/// be unique, so a SHA-256 of the claims stands in for the library's own hash.
fn encode_jwt(signer: &KeyPair, subject: &str, name: &str, iat: i64, nats: Value) -> String {
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

struct User {
    kp: KeyPair,
    creds: String,
}

impl User {
    fn public_key(&self) -> String {
        self.kp.public_key()
    }
}

struct Account {
    name: String,
    kp: KeyPair,
    jetstream: bool,
    iat: i64,
    revocations: BTreeMap<String, i64>,
}

impl Account {
    fn new(name: &str, jetstream: bool) -> Self {
        Self {
            name: name.to_string(),
            kp: KeyPair::new_account(),
            jetstream,
            // Back-dated so a later update (revocation) always carries a newer iat.
            iat: unix_now() - 120,
            revocations: BTreeMap::new(),
        }
    }

    fn public_key(&self) -> String {
        self.kp.public_key()
    }

    fn jwt(&self, operator: &KeyPair) -> String {
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
        encode_jwt(operator, &self.public_key(), &self.name, self.iat, nats)
    }

    /// Issues a user in this account with no permission restrictions, back-dated one
    /// minute so a revocation stamped "now" covers it.
    fn user(&self, name: &str) -> User {
        let kp = KeyPair::new_user();
        let nats = json!({
            "type": "user", "version": 2,
            "pub": {}, "sub": {}, "subs": -1, "data": -1, "payload": -1,
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

struct Operator {
    name: String,
    kp: KeyPair,
    sys: Account,
    sys_user: User,
}

impl Operator {
    fn new(name: &str) -> Self {
        let sys = Account::new(&format!("{name}-SYS"), false);
        let sys_user = sys.user(&format!("{name}-sys-user"));
        Self {
            name: name.to_string(),
            kp: KeyPair::new_operator(),
            sys,
            sys_user,
        }
    }

    fn jwt(&self) -> String {
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

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind probe port")
        .local_addr()
        .expect("probe addr")
        .port()
}

async fn wait_for<F, Fut>(what: &str, limit: Duration, mut probe: F) -> Duration
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

async fn http_get(port: u16, path: &str) -> Option<String> {
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

struct Server {
    name: String,
    bin: PathBuf,
    conf: PathBuf,
    log: PathBuf,
    client_port: u16,
    http_port: u16,
    child: Option<Child>,
}

/// How a server resolves account JWTs.
enum Resolver {
    /// Preloaded, read-only: enough for a box server whose accounts never change here.
    Memory,
    /// The directory resolver, which accepts signed account updates over `$SYS`.
    Full,
}

impl Server {
    #[allow(clippy::too_many_arguments)]
    fn configure(
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
        let client_port = free_port();
        let http_port = free_port();
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
             listen: \"127.0.0.1:{client_port}\"\n\
             http: \"127.0.0.1:{http_port}\"\n\
             log_file: \"{log}\"\n\
             logtime: true\n\
             jetstream {{ store_dir: \"{js}\", domain: \"{domain}\" }}\n\
             operator: \"{op}\"\n\
             system_account: \"{sys}\"\n\
             {resolver}\n\
             resolver_preload {{\n{preload}}}\n\
             {extra}\n",
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
            client_port,
            http_port,
            child: None,
        }
    }

    fn url(&self) -> String {
        format!("nats://127.0.0.1:{}", self.client_port)
    }

    async fn start(&mut self) {
        let child = Command::new(&self.bin)
            .arg("-c")
            .arg(&self.conf)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn nats-server");
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
    async fn stop(&mut self) {
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

    fn log_lines(&self, needles: &[&str]) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter(|line| needles.iter().any(|needle| line.contains(needle)))
            .map(str::to_string)
            .collect()
    }

    fn print_log(&self, test: u8, needles: &[&str]) {
        for line in self.log_lines(needles) {
            note(test, format!("  {} log: {line}", self.name));
        }
    }

    async fn leafs(&self) -> Vec<(String, String)> {
        leafs_at(self.http_port).await
    }

    async fn wait_leaf_names(&self, want: &[&str], limit: Duration) -> Duration {
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
async fn leafs_at(http_port: u16) -> Vec<(String, String)> {
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
struct Proxy {
    port: u16,
    cut: Arc<AtomicBool>,
    conns: Arc<std::sync::Mutex<Vec<JoinHandle<()>>>>,
    accept: JoinHandle<()>,
}

impl Proxy {
    async fn start(upstream: u16) -> Proxy {
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

    fn cut(&self) {
        self.cut.store(true, Ordering::SeqCst);
        for relay in self.conns.lock().expect("proxy conns").drain(..) {
            relay.abort();
        }
    }

    fn restore(&self) {
        self.cut.store(false, Ordering::SeqCst);
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.accept.abort();
        self.cut();
    }
}

// ---------------------------------------------------------------------------------
// The three-server rig
// ---------------------------------------------------------------------------------

struct Rig {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    hub: Server,
    a: Server,
    b: Server,
    proxy_a: Proxy,
    proxy_b: Proxy,
    hub_operator: Operator,
    /// The one per-user account on the hub that both leafs bind into.
    user_account: Account,
    leaf_user_a: User,
    hub_client: User,
    a_local: User,
    a_fed: User,
    b_local: User,
    b_fed: User,
}

impl Rig {
    async fn start(label: &str) -> Rig {
        let bin = nats_server_bin();
        let tmp = tempfile::Builder::new()
            .prefix(&format!("ck-nats-rig-{label}-"))
            .tempdir()
            .expect("rig tempdir");
        let root = tmp.path().to_path_buf();

        let hub_operator = Operator::new("hub-operator");
        let user_account = Account::new("user-u", true);
        let leaf_user_a = user_account.user("leaf-a");
        let leaf_user_b = user_account.user("leaf-b");
        let hub_client = user_account.user("hub-client");

        let hub_leaf_port = free_port();
        let mut hub = Server::configure(
            &bin,
            &root,
            "hub",
            "hub",
            &hub_operator,
            &[&user_account],
            Resolver::Full,
            // `no_advertise`: by default the hub puts its own leaf listen address in
            // the INFO it sends each leaf, and the leaf adds that address to its
            // remote's URL list. A leaf that lost its proxied link then redials the
            // hub directly and the split never happens (measured; see the results
            // document).
            &format!("leafnodes {{ listen: \"127.0.0.1:{hub_leaf_port}\", no_advertise: true }}"),
        );
        hub.start().await;
        let proxy_a = Proxy::start(hub_leaf_port).await;
        let proxy_b = Proxy::start(hub_leaf_port).await;

        let (a, a_local, a_fed) = Self::box_server(&bin, &root, "a", &leaf_user_a, &proxy_a);
        let (b, b_local, b_fed) = Self::box_server(&bin, &root, "b", &leaf_user_b, &proxy_b);
        let (mut a, mut b) = (a, b);
        a.start().await;
        b.start().await;
        hub.wait_leaf_names(&["box-a", "box-b"], Duration::from_secs(20))
            .await;

        Rig {
            _tmp: tmp,
            root,
            hub,
            a,
            b,
            proxy_a,
            proxy_b,
            hub_operator,
            user_account,
            leaf_user_a,
            hub_client,
            a_local,
            a_fed,
            b_local,
            b_fed,
        }
    }

    /// One box: its own operator, a LOCAL account and a FED account, and a leaf remote
    /// that binds ONLY the FED account to the hub, authenticating as `leaf_user`.
    fn box_server(
        bin: &Path,
        root: &Path,
        tag: &str,
        leaf_user: &User,
        proxy: &Proxy,
    ) -> (Server, User, User) {
        let operator = Operator::new(&format!("box-{tag}-operator"));
        let local = Account::new(&format!("box-{tag}-local"), true);
        let fed = Account::new(&format!("box-{tag}-fed"), true);
        let creds_path = root.join(format!("leaf-{tag}.creds"));
        std::fs::write(&creds_path, &leaf_user.creds).expect("leaf creds");
        let extra = format!(
            "leafnodes {{\n  remotes: [\n    {{ url: \"nats-leaf://127.0.0.1:{port}\", \
             credentials: \"{creds}\", account: \"{fed}\" }}\n  ]\n}}",
            port = proxy.port,
            creds = creds_path.display(),
            fed = fed.public_key(),
        );
        let server = Server::configure(
            bin,
            root,
            &format!("box-{tag}"),
            tag,
            &operator,
            &[&local, &fed],
            Resolver::Memory,
            &extra,
        );
        let local_user = local.user(&format!("box-{tag}-local-client"));
        let fed_user = fed.user(&format!("box-{tag}-fed-client"));
        (server, local_user, fed_user)
    }

    fn leaf_creds_path(&self, tag: &str) -> PathBuf {
        self.root.join(format!("leaf-{tag}.creds"))
    }

    /// Signs the user account's current claims with the hub operator and pushes them
    /// through the hub's `$SYS.REQ.CLAIMS.UPDATE`, returning the server's reply.
    async fn push_user_account(&mut self) -> String {
        self.user_account.iat = (self.user_account.iat + 1).max(unix_now());
        let jwt = self.user_account.jwt(&self.hub_operator.kp);
        let sys = connect(&self.hub.url(), &self.hub_operator.sys_user).await;
        let reply = sys
            .request("$SYS.REQ.CLAIMS.UPDATE", jwt.into())
            .await
            .expect("claims update request");
        String::from_utf8_lossy(&reply.payload).into_owned()
    }
}

async fn connect(url: &str, user: &User) -> async_nats::Client {
    async_nats::ConnectOptions::with_credentials(&user.creds)
        .expect("parse creds")
        .connect(url)
        .await
        .unwrap_or_else(|err| panic!("connect {url}: {err}"))
}

/// Core NATS is at-most-once and subscription interest crosses leafs asynchronously,
/// so the first cross-machine message is re-published until it lands. Returns how
/// many attempts that took.
async fn publish_until_received(
    publisher: &async_nats::Client,
    subject: &str,
    subscriber: &mut async_nats::Subscriber,
    limit: Duration,
) -> u32 {
    let start = Instant::now();
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        publisher
            .publish(subject.to_string(), format!("probe-{attempt}").into())
            .await
            .expect("publish");
        publisher.flush().await.expect("flush");
        let deadline = Instant::now() + Duration::from_millis(200);
        while let Ok(Some(msg)) = tokio::time::timeout_at(deadline.into(), subscriber.next()).await
        {
            if msg.subject.as_str() == subject {
                return attempt;
            }
        }
        assert!(
            start.elapsed() < limit,
            "no message on {subject} after {attempt} attempts in {limit:?}"
        );
    }
}

/// Subscribes to `subject` and returns only once the server has registered the
/// subscription.
///
/// `async_nats::Client::flush` only drains the client's socket write buffer; it sends no
/// PING, so it does not wait for the server to process the SUB. The server reads each
/// connection independently, so a PUB sent right afterwards on ANOTHER connection can be
/// processed first and routed to nobody (measured in the revocation test: the server's
/// trace showed the other client's PUB 7 µs before this client's SUB). The server does
/// process one connection's commands in order, so publishing a marker on the same
/// connection and receiving it back proves the SUB is in place. The marker is consumed
/// here and never reaches the caller.
async fn subscribe_confirmed(client: &async_nats::Client, subject: &str) -> async_nats::Subscriber {
    subscribe_confirmed_via(client, subject, subject).await
}

/// `subscribe_confirmed` for a wildcard subscription, which cannot be published to:
/// the marker goes to `marker_subject`, a literal subject the wildcard covers.
async fn subscribe_confirmed_via(
    client: &async_nats::Client,
    subject: &str,
    marker_subject: &str,
) -> async_nats::Subscriber {
    let mut sub = client
        .subscribe(subject.to_string())
        .await
        .expect("subscribe");
    let marker = format!("subscription-registered-{}", client.new_inbox());
    client
        .publish(marker_subject.to_string(), marker.clone().into())
        .await
        .expect("publish subscription marker");
    client.flush().await.expect("flush");
    let msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .unwrap_or_else(|_| panic!("the server never echoed the marker on {subject}"))
        .expect("subscription closed before its marker arrived");
    assert_eq!(
        msg.payload.as_ref(),
        marker.as_bytes(),
        "the first message on a fresh subscription must be its own marker"
    );
    sub
}

/// Everything that arrives on `sub` within `window`, as `(subject, payload)`.
async fn drain(sub: &mut async_nats::Subscriber, window: Duration) -> Vec<(String, String)> {
    let deadline = Instant::now() + window;
    let mut got = Vec::new();
    while let Ok(Some(msg)) = tokio::time::timeout_at(deadline.into(), sub.next()).await {
        got.push((
            msg.subject.to_string(),
            String::from_utf8_lossy(&msg.payload).into_owned(),
        ));
    }
    got
}

async fn stream_messages(js: &jetstream::Context, name: &str) -> u64 {
    match js.get_stream(name).await {
        Ok(stream) => stream.cached_info().state.messages,
        Err(_) => 0,
    }
}

/// `(sequence, subject, payload)` for every message currently in the stream.
async fn stream_contents(js: &jetstream::Context, name: &str) -> Vec<(u64, String, String)> {
    let stream = js.get_stream(name).await.expect("get stream");
    let state = stream.cached_info().state.clone();
    let mut out = Vec::new();
    if state.messages == 0 {
        return out;
    }
    for seq in state.first_sequence..=state.last_sequence {
        if let Ok(msg) = stream.get_raw_message(seq).await {
            out.push((
                seq,
                msg.subject.to_string(),
                String::from_utf8_lossy(&msg.payload).into_owned(),
            ));
        }
    }
    out
}

async fn js_publish(
    js: &jetstream::Context,
    subject: &str,
    payload: &str,
) -> Result<String, String> {
    let ack = tokio::time::timeout(Duration::from_secs(5), async {
        js.publish(subject.to_string(), payload.to_string().into())
            .await
            .map_err(|e| format!("publish: {e}"))?
            .await
            .map_err(|e| format!("ack: {e}"))
    })
    .await
    .map_err(|_| "no ack within 5s".to_string())??;
    Ok(format!(
        "stream={} seq={} domain={}",
        ack.stream, ack.sequence, ack.domain
    ))
}

// ---------------------------------------------------------------------------------
// 1. Leaf routing into a per-user hub account
// ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the nats-server binary"]
async fn rig1_leaf_routing_into_per_user_hub_account() {
    let _serial = RIG_LOCK.lock().await;
    let rig = Rig::start("routing").await;
    let user_pub = rig.user_account.public_key();

    let leafs = rig.hub.leafs().await;
    for (name, account) in &leafs {
        note(
            1,
            format!("hub /leafz: leaf {name} bound to hub account {account}"),
        );
    }
    assert_eq!(leafs.len(), 2, "both boxes must hold a leaf to the hub");
    assert!(
        leafs.iter().all(|(_, account)| *account == user_pub),
        "both leafs must bind into the one per-user hub account {user_pub}"
    );

    let a = connect(&rig.a.url(), &rig.a_fed).await;
    let b = connect(&rig.b.url(), &rig.b_fed).await;
    let mut inbox = b.subscribe("ck.b.peer.>").await.expect("subscribe");
    b.flush().await.expect("flush");
    let attempts = publish_until_received(
        &a,
        "ck.b.peer.agent_rig1",
        &mut inbox,
        Duration::from_secs(10),
    )
    .await;
    note(
        1,
        format!(
            "A FED -> B FED on ck.b.peer.agent_rig1 arrived after {attempts} publish attempt(s)"
        ),
    );

    for i in 0..5 {
        a.publish("ck.b.peer.agent_rig1", format!("steady-{i}").into())
            .await
            .expect("publish");
    }
    a.flush().await.expect("flush");
    let steady: Vec<String> = drain(&mut inbox, Duration::from_secs(2))
        .await
        .into_iter()
        .map(|(_, payload)| payload)
        .filter(|payload| payload.starts_with("steady-"))
        .collect();
    note(1, format!("steady-state messages at B: {steady:?}"));
    assert_eq!(
        steady,
        (0..5).map(|i| format!("steady-{i}")).collect::<Vec<_>>(),
        "every steady-state message must arrive at B in order"
    );
    rig.hub.print_log(1, &["Leafnode connection created"]);
    note(
        1,
        "RESULT: PASS - A's FED account reaches B's FED account through the per-user hub account",
    );
}

// ---------------------------------------------------------------------------------
// 2. Local subjects stay off the leaf
// ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the nats-server binary"]
async fn rig2_local_subjects_stay_off_the_leaf() {
    let _serial = RIG_LOCK.lock().await;
    let rig = Rig::start("local").await;

    let hub = connect(&rig.hub.url(), &rig.hub_client).await;
    let a_local = connect(&rig.a.url(), &rig.a_local).await;
    let a_fed = connect(&rig.a.url(), &rig.a_fed).await;
    let b_fed = connect(&rig.b.url(), &rig.b_fed).await;
    let b_local = connect(&rig.b.url(), &rig.b_local).await;

    // The widest possible hub-side interest in the user account. Each `>` is confirmed
    // registered before anything is published, so an empty drain below means nothing
    // arrived, not that the subscription was not yet in place.
    let mut hub_all = subscribe_confirmed_via(&hub, ">", "rig.marker.hub").await;
    let mut b_fed_all = subscribe_confirmed_via(&b_fed, ">", "rig.marker.b_fed").await;
    let mut b_local_all = subscribe_confirmed_via(&b_local, ">", "rig.marker.b_local").await;

    // Positive control: the hub's `>` really does pull A's FED traffic across, so a
    // silent hub subscription below means the subject stayed home, not a dead probe.
    let attempts = publish_until_received(
        &a_fed,
        "ck.a.peer.fed_control",
        &mut hub_all,
        Duration::from_secs(10),
    )
    .await;
    note(
        2,
        format!("control: A FED publish reached hub `>` after {attempts} attempt(s)"),
    );

    for i in 0..20 {
        for subject in [
            "ck.a.peer.agent_local",
            "ck.b.peer.agent_local",
            "ck.a.room.rm_local",
        ] {
            a_local
                .publish(subject, format!("LOCAL-{i}").into())
                .await
                .expect("local publish");
        }
    }
    a_local.flush().await.expect("flush");
    a_fed
        .publish("ck.a.peer.fed_control", "after-local".into())
        .await
        .expect("publish");
    a_fed.flush().await.expect("flush");

    let hub_seen = drain(&mut hub_all, Duration::from_secs(2)).await;
    let b_fed_seen = drain(&mut b_fed_all, Duration::from_millis(500)).await;
    let b_local_seen = drain(&mut b_local_all, Duration::from_millis(500)).await;
    let leaked = |seen: &[(String, String)]| {
        seen.iter()
            .filter(|(_, payload)| payload.starts_with("LOCAL-"))
            .count()
    };
    note(
        2,
        format!(
            "after 60 LOCAL publishes on A: hub `>` saw {} message(s) ({} LOCAL), B FED `>` saw {} LOCAL, B LOCAL `>` saw {} LOCAL",
            hub_seen.len(),
            leaked(&hub_seen),
            leaked(&b_fed_seen),
            leaked(&b_local_seen)
        ),
    );
    assert!(
        hub_seen.iter().any(|(_, payload)| payload == "after-local"),
        "the trailing FED control must reach the hub, or the silence proves nothing"
    );
    assert_eq!(
        leaked(&hub_seen),
        0,
        "A LOCAL traffic reached the hub: {hub_seen:?}"
    );
    assert_eq!(leaked(&b_fed_seen), 0, "A LOCAL traffic reached B FED");
    assert_eq!(leaked(&b_local_seen), 0, "A LOCAL traffic reached B LOCAL");

    // Reverse direction: a hub publish on a subject A's LOCAL account listens to.
    let mut a_local_all = subscribe_confirmed_via(&a_local, ">", "rig.marker.a_local").await;
    let mut a_fed_peer = a_fed.subscribe("ck.a.peer.>").await.expect("a fed sub");
    a_fed.flush().await.expect("flush");
    let attempts = publish_until_received(
        &hub,
        "ck.a.peer.agent_local",
        &mut a_fed_peer,
        Duration::from_secs(10),
    )
    .await;
    let a_local_seen = drain(&mut a_local_all, Duration::from_secs(1)).await;
    note(
        2,
        format!(
            "reverse: hub publish reached A FED after {attempts} attempt(s); A LOCAL `>` saw {} message(s)",
            a_local_seen.len()
        ),
    );
    assert!(
        a_local_seen.is_empty(),
        "hub traffic reached A LOCAL: {a_local_seen:?}"
    );
    note(
        2,
        "RESULT: PASS - nothing in a box's LOCAL account crosses the leaf in either direction",
    );
}

// ---------------------------------------------------------------------------------
// 3. Stream sourcing across a split
// ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the nats-server binary"]
async fn rig3_stream_sourcing_across_a_split() {
    let _serial = RIG_LOCK.lock().await;
    let mut rig = Rig::start("split").await;
    let recipient = "ck.b.peer.agent_rig3";

    let hub_js = jetstream::new(connect(&rig.hub.url(), &rig.hub_client).await);
    let a_js = jetstream::new(connect(&rig.a.url(), &rig.a_fed).await);

    // Sender-side outbox on A. Its subjects are sender-namespaced (`ckout.a.>`) so no
    // other stream's subject interest overlaps it; the hub sources it and rewrites the
    // subject back into the recipient's inbox family.
    a_js.create_stream(stream::Config {
        name: "CK_A_OUTBOX".into(),
        subjects: vec!["ckout.a.>".into()],
        storage: stream::StorageType::File,
        discard: stream::DiscardPolicy::New,
        max_age: Duration::from_secs(3 * 24 * 3600),
        ..Default::default()
    })
    .await
    .expect("create A outbox");

    // The hub holds every box's inbox subjects for the user, as the design's
    // store-and-forward hop does, and also sources A's outbox.
    hub_js
        .create_stream(stream::Config {
            name: "CK_HUB_PEER".into(),
            subjects: vec!["ck.*.peer.>".into()],
            storage: stream::StorageType::File,
            discard: stream::DiscardPolicy::New,
            max_age: Duration::from_secs(3 * 24 * 3600),
            sources: Some(vec![stream::Source {
                name: "CK_A_OUTBOX".into(),
                external: Some(stream::External {
                    api_prefix: "$JS.a.API".into(),
                    delivery_prefix: None,
                }),
                subject_transforms: vec![stream::SubjectTransform {
                    source: "ckout.a.>".into(),
                    destination: "ck.>".into(),
                }],
                ..Default::default()
            }]),
            ..Default::default()
        })
        .await
        .expect("create hub stream");

    // B's inbox binds no subjects of its own: it only sources its filter from the hub,
    // so a message reaches it by exactly one path.
    let b_config = stream::Config {
        name: "CK_B_INBOX".into(),
        storage: stream::StorageType::File,
        sources: Some(vec![stream::Source {
            name: "CK_HUB_PEER".into(),
            filter_subject: Some("ck.b.peer.>".into()),
            external: Some(stream::External {
                api_prefix: "$JS.hub.API".into(),
                delivery_prefix: None,
            }),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let b_js = jetstream::new(connect(&rig.b.url(), &rig.b_fed).await);
    b_js.create_stream(b_config).await.expect("create B inbox");

    // Warm-up: wait until the hub stream's subject interest has crossed A's leaf.
    wait_for(
        "hub stream reachable from A",
        Duration::from_secs(10),
        || {
            let js = a_js.clone();
            async move { js_publish(&js, "ck.z.peer.warmup", "warmup").await.is_ok() }
        },
    )
    .await;

    let mut expected = Vec::new();
    for i in 1..=3 {
        let payload = format!("m{i:02}");
        let ack = js_publish(&a_js, recipient, &payload)
            .await
            .expect("publish");
        note(3, format!("connected: {payload} acked {ack}"));
        expected.push(payload);
    }
    let js = b_js.clone();
    wait_for("B inbox == 3", Duration::from_secs(20), || {
        let js = js.clone();
        async move { stream_messages(&js, "CK_B_INBOX").await == 3 }
    })
    .await;

    // Phase A: network split between B and the hub (both servers keep running).
    rig.proxy_b.cut();
    rig.hub
        .wait_leaf_names(&["box-a"], Duration::from_secs(20))
        .await;
    note(3, "split: B's leaf link cut; hub /leafz shows only box-a");
    for i in 4..=8 {
        let payload = format!("m{i:02}");
        let ack = js_publish(&a_js, recipient, &payload)
            .await
            .expect("publish during split");
        note(3, format!("during B split: {payload} acked {ack}"));
        expected.push(payload);
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    let during = stream_messages(&b_js, "CK_B_INBOX").await;
    note(3, format!("B inbox during split holds {during} message(s)"));
    assert_eq!(during, 3, "nothing can reach B while its link is cut");
    rig.proxy_b.restore();
    let js = b_js.clone();
    let caught_up = wait_for(
        "B inbox == 8 after restore",
        Duration::from_secs(120),
        || {
            let js = js.clone();
            async move { stream_messages(&js, "CK_B_INBOX").await == 8 }
        },
    )
    .await;
    note(
        3,
        format!("link restored: B caught up to 8 in {caught_up:?}"),
    );

    // Phase B: B's server itself is down (a closed laptop), then restarts from disk.
    rig.b.stop().await;
    rig.hub
        .wait_leaf_names(&["box-a"], Duration::from_secs(20))
        .await;
    for i in 9..=10 {
        let payload = format!("m{i:02}");
        let ack = js_publish(&a_js, recipient, &payload)
            .await
            .expect("publish while B down");
        note(
            3,
            format!("while B's server is stopped: {payload} acked {ack}"),
        );
        expected.push(payload);
    }
    rig.b.start().await;
    let b_js = jetstream::new(connect(&rig.b.url(), &rig.b_fed).await);
    let js = b_js.clone();
    let caught_up = wait_for(
        "B inbox == 10 after restart",
        Duration::from_secs(120),
        || {
            let js = js.clone();
            async move { stream_messages(&js, "CK_B_INBOX").await == 10 }
        },
    )
    .await;
    note(3, format!("B restarted: caught up to 10 in {caught_up:?}"));

    // Phase C: the SENDER is split and publishes straight to the recipient subject,
    // which is what the design's text describes (A publishes `ck.{b}.peer.…`).
    rig.proxy_a.cut();
    rig.hub
        .wait_leaf_names(&["box-b"], Duration::from_secs(20))
        .await;
    let direct = js_publish(&a_js, recipient, "lost-11").await;
    note(
        3,
        format!("A split, direct publish to {recipient}: {direct:?} (no stream on A binds it)"),
    );
    rig.proxy_a.restore();
    rig.hub
        .wait_leaf_names(&["box-a", "box-b"], Duration::from_secs(20))
        .await;

    // Phase D: the sender is split but writes to its local outbox, which the hub sources.
    let outbox_subject = "ckout.a.b.peer.agent_rig3";
    let ack = js_publish(&a_js, outbox_subject, "o00")
        .await
        .expect("outbox publish");
    note(3, format!("connected: o00 into A outbox acked {ack}"));
    expected.push("o00".into());
    let js = b_js.clone();
    wait_for(
        "B inbox == 11 (outbox path live)",
        Duration::from_secs(60),
        || {
            let js = js.clone();
            async move { stream_messages(&js, "CK_B_INBOX").await == 11 }
        },
    )
    .await;
    rig.proxy_a.cut();
    rig.hub
        .wait_leaf_names(&["box-b"], Duration::from_secs(20))
        .await;
    for i in 1..=3 {
        let payload = format!("o{i:02}");
        let ack = js_publish(&a_js, outbox_subject, &payload)
            .await
            .expect("outbox publish during A split");
        note(
            3,
            format!("during A split: {payload} into A outbox acked {ack}"),
        );
        expected.push(payload);
    }
    rig.proxy_a.restore();
    let js = b_js.clone();
    let caught_up = wait_for(
        "B inbox == 14 after A restore",
        Duration::from_secs(120),
        || {
            let js = js.clone();
            async move { stream_messages(&js, "CK_B_INBOX").await == 14 }
        },
    )
    .await;
    note(3, format!("A restored: B caught up to 14 in {caught_up:?}"));

    // Settle, then read B's inbox back: every message exactly once, in order.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let contents = stream_contents(&b_js, "CK_B_INBOX").await;
    let got: Vec<String> = contents.iter().map(|(_, _, p)| p.clone()).collect();
    for (seq, subject, payload) in &contents {
        note(3, format!("B inbox seq {seq}: {subject} {payload}"));
    }
    let hub_contents = stream_contents(&hub_js, "CK_HUB_PEER").await;
    note(
        3,
        format!(
            "hub stream holds {} message(s); `lost-11` present: {}",
            hub_contents.len(),
            hub_contents.iter().any(|(_, _, p)| p == "lost-11")
        ),
    );
    rig.a.print_log(
        3,
        &["Leafnode connection created", "Leafnode connection closed"],
    );
    rig.b.print_log(
        3,
        &["Leafnode connection created", "Leafnode connection closed"],
    );
    assert_eq!(
        got, expected,
        "B must hold every message exactly once, in order"
    );
    assert!(
        direct.is_err(),
        "a direct publish during a sender split had nowhere to land"
    );
    assert!(
        !got.iter().any(|p| p == "lost-11"),
        "the direct publish during the sender split must not reappear"
    );
    note(
        3,
        "RESULT: PASS - recipient splits (link cut and server restart) and a sender split through an outbox all caught up exactly once, in order; a direct publish during a sender split is lost",
    );
}

// ---------------------------------------------------------------------------------
// 4. Subject-filtered purge of one recipient's inbox
// ---------------------------------------------------------------------------------

async fn per_recipient_counts(stream: &stream::Stream) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    let subjects = stream
        .info_with_subjects("ck.*.peer.>")
        .await
        .expect("info with subjects");
    let mut subjects = std::pin::pin!(subjects);
    while let Some(item) = subjects.next().await {
        let (subject, count) = item.expect("subject count");
        let recipient = subject.split('.').nth(1).unwrap_or_default().to_string();
        *counts.entry(recipient).or_insert(0) += count;
    }
    counts
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the nats-server binary"]
async fn rig4_subject_filtered_purge_of_one_inbox() {
    let _serial = RIG_LOCK.lock().await;
    let rig = Rig::start("purge").await;
    let hub_js = jetstream::new(connect(&rig.hub.url(), &rig.hub_client).await);
    let stream = hub_js
        .create_stream(stream::Config {
            name: "CK_HUB_PEER".into(),
            subjects: vec!["ck.*.peer.>".into()],
            storage: stream::StorageType::File,
            ..Default::default()
        })
        .await
        .expect("create hub stream");

    let recipients = ["b", "c", "d"];
    for i in 0..12 {
        let recipient = recipients[i % 3];
        js_publish(
            &hub_js,
            &format!("ck.{recipient}.peer.agent_{i}"),
            &format!("{recipient}-{i}"),
        )
        .await
        .expect("publish");
    }
    let before = per_recipient_counts(&stream).await;
    note(
        4,
        format!("before purge, messages per recipient: {before:?}"),
    );

    let purge = stream
        .purge()
        .filter("ck.b.peer.>")
        .await
        .expect("filtered purge");
    note(
        4,
        format!(
            "purge filter ck.b.peer.> -> success={} purged={}",
            purge.success, purge.purged
        ),
    );
    let after = per_recipient_counts(&stream).await;
    note(4, format!("after purge, messages per recipient: {after:?}"));
    let remaining: Vec<String> = stream_contents(&hub_js, "CK_HUB_PEER")
        .await
        .into_iter()
        .map(|(seq, _, payload)| format!("{seq}:{payload}"))
        .collect();
    note(4, format!("remaining seq:payload: {remaining:?}"));

    assert_eq!(purge.purged, 4, "exactly B's four messages are purged");
    assert_eq!(after.get("b"), None, "B's inbox is empty");
    assert_eq!(after.get("c"), before.get("c"), "C is untouched");
    assert_eq!(after.get("d"), before.get("d"), "D is untouched");
    assert!(
        remaining.iter().all(|entry| !entry.contains(":b-")),
        "no B payload survives"
    );
    note(
        4,
        "RESULT: PASS - a subject-filtered purge removes one recipient's messages and nothing else",
    );
}

// ---------------------------------------------------------------------------------
// 5. SignatureCB and keeping the leaf key off disk
// ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the nats-server binary"]
async fn rig5_leaf_signature_callback_and_key_on_disk() {
    let _serial = RIG_LOCK.lock().await;
    let bin = nats_server_bin();
    let version = Command::new(&bin)
        .arg("--version")
        .output()
        .await
        .expect("nats-server --version");
    let version = String::from_utf8_lossy(&version.stdout).trim().to_string();
    note(5, format!("binary: {} reports `{version}`", bin.display()));
    assert!(version.contains("v2.15.0"), "rig is pinned to v2.15.0");

    // The config file has no key for a signing callback: `SignatureCB` exists only on
    // the Go `RemoteLeafOpts` struct. Offer the obvious spellings and let the parser
    // answer.
    let tmp = tempfile::tempdir().expect("tempdir");
    for key in ["signature_cb", "signature", "sign_callback"] {
        let conf = tmp.path().join(format!("{key}.conf"));
        std::fs::write(
            &conf,
            format!(
                "listen: \"127.0.0.1:{}\"\nleafnodes {{ remotes: [ {{ url: \"nats-leaf://127.0.0.1:1\", {key}: \"vault\" }} ] }}\n",
                free_port()
            ),
        )
        .expect("conf");
        let out = Command::new(&bin)
            .arg("-t")
            .arg("-c")
            .arg(&conf)
            .output()
            .await
            .expect("nats-server -t");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        note(
            5,
            format!(
                "config remote key `{key}`: exit={:?} output={}",
                out.status.code(),
                text.trim()
            ),
        );
        assert!(
            !out.status.success(),
            "config key `{key}` must not be accepted"
        );
    }

    // The stock alternative is a creds file. It is read at EVERY connect and
    // reconnect, so it must exist whenever the leaf dials, not only at startup.
    let rig = Rig::start("creds").await;
    let creds = rig.leaf_creds_path("a");
    std::fs::remove_file(&creds).expect("remove creds");
    note(
        5,
        "A connected; its creds file is now deleted; cutting A's link",
    );
    rig.proxy_a.cut();
    rig.hub
        .wait_leaf_names(&["box-b"], Duration::from_secs(20))
        .await;
    rig.proxy_a.restore();
    tokio::time::sleep(Duration::from_secs(4)).await;
    let leafs_without_file = rig.hub.leafs().await;
    note(
        5,
        format!(
            "4s after restoring the link with no creds file, hub leafs: {:?}",
            leafs_without_file
                .iter()
                .map(|(n, _)| n)
                .collect::<Vec<_>>()
        ),
    );
    rig.a.print_log(5, &["redentials", "no such file"]);
    assert!(
        !leafs_without_file.iter().any(|(name, _)| name == "box-a"),
        "the leaf cannot reconnect once its creds file is gone"
    );
    std::fs::write(&creds, &rig.leaf_user_a.creds).expect("rewrite creds");
    let back = rig
        .hub
        .wait_leaf_names(&["box-a", "box-b"], Duration::from_secs(20))
        .await;
    note(
        5,
        format!("creds file written back: leaf reconnected in {back:?}"),
    );
    note(
        5,
        "RESULT: SignatureCB is Go-API only (server/opts.go RemoteLeafOpts, v2.15.0); stock config needs creds/nkey material readable at every (re)connect",
    );
}

// ---------------------------------------------------------------------------------
// 6. Revoking one user
// ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the nats-server binary"]
async fn rig6_revoking_one_user() {
    let _serial = RIG_LOCK.lock().await;
    let mut rig = Rig::start("revoke").await;
    let u1 = rig.user_account.user("client-u1");
    let u2 = rig.user_account.user("client-u2");

    let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = events.clone();
    let c1 = async_nats::ConnectOptions::with_credentials(&u1.creds)
        .expect("creds")
        .event_callback(move |event| {
            let sink = sink.clone();
            async move {
                sink.lock().expect("events").push(event.to_string());
            }
        })
        .connect(rig.hub.url())
        .await
        .expect("u1 connect");
    let c2 = connect(&rig.hub.url(), &u2).await;
    let mut c2_sub = subscribe_confirmed(&c2, "rev.check").await;
    c1.publish("rev.check", "before".into()).await.expect("pub");
    c1.flush().await.expect("flush");
    let before = drain(&mut c2_sub, Duration::from_millis(500)).await;
    note(
        6,
        format!("before revocation: u2 received {before:?} from u1"),
    );
    assert_eq!(before.len(), 1, "both users work before revocation");

    // Revoke u1 only, by account JWT revocation pushed to the hub's resolver.
    rig.user_account
        .revocations
        .insert(u1.public_key(), unix_now());
    let reply = rig.push_user_account().await;
    note(
        6,
        format!("revoke u1: $SYS.REQ.CLAIMS.UPDATE reply {reply}"),
    );

    let sink = events.clone();
    wait_for("u1 disconnect event", Duration::from_secs(10), move || {
        let sink = sink.clone();
        async move {
            sink.lock()
                .expect("events")
                .iter()
                .any(|e| e.contains("disconnected"))
        }
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    note(
        6,
        format!("u1 client events: {:?}", events.lock().expect("events")),
    );
    note(
        6,
        format!("u1 connection state: {:?}", c1.connection_state()),
    );
    let fresh = async_nats::ConnectOptions::with_credentials(&u1.creds)
        .expect("creds")
        .connect(rig.hub.url())
        .await;
    let refused = fresh.as_ref().err().map(|e| e.to_string());
    note(6, format!("fresh connect as u1: {refused:?}"));
    assert!(
        refused.is_some(),
        "a revoked user's new connection must be refused"
    );
    assert_ne!(
        c1.connection_state(),
        async_nats::connection::State::Connected,
        "u1's own reconnects must fail"
    );

    // u2 is untouched: its existing connection and a fresh one both still work.
    c2.publish("rev.check", "after".into()).await.expect("pub");
    c2.flush().await.expect("flush");
    let after = drain(&mut c2_sub, Duration::from_millis(500)).await;
    let c2_fresh = async_nats::ConnectOptions::with_credentials(&u2.creds)
        .expect("creds")
        .connect(rig.hub.url())
        .await;
    note(
        6,
        format!(
            "u2 after u1's revocation: existing connection received {after:?}, fresh connect ok={}",
            c2_fresh.is_ok()
        ),
    );
    assert_eq!(after.len(), 1, "u2's existing connection keeps working");
    assert!(c2_fresh.is_ok(), "u2 can still connect");

    // The design's case: revoke one machine's leaf credential. Box A's leaf must be
    // dropped and refused; box B's leaf, in the same account, keeps routing.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    rig.user_account
        .revocations
        .insert(rig.leaf_user_a.public_key(), unix_now());
    let reply = rig.push_user_account().await;
    note(
        6,
        format!("revoke leaf-a: $SYS.REQ.CLAIMS.UPDATE reply {reply}"),
    );
    let dropped = rig
        .hub
        .wait_leaf_names(&["box-b"], Duration::from_secs(10))
        .await;
    note(
        6,
        format!("hub /leafz shows only box-b {dropped:?} after the update"),
    );
    tokio::time::sleep(Duration::from_secs(4)).await;
    let leafs = rig.hub.leafs().await;
    note(
        6,
        format!(
            "4s later hub leafs: {:?}",
            leafs.iter().map(|(n, _)| n).collect::<Vec<_>>()
        ),
    );
    assert!(
        !leafs.iter().any(|(name, _)| name == "box-a"),
        "box-a's leaf must stay refused"
    );
    let hub = connect(&rig.hub.url(), &rig.hub_client).await;
    let b = connect(&rig.b.url(), &rig.b_fed).await;
    let mut b_inbox = b.subscribe("ck.b.peer.>").await.expect("sub");
    b.flush().await.expect("flush");
    let attempts = publish_until_received(
        &hub,
        "ck.b.peer.agent_rig6",
        &mut b_inbox,
        Duration::from_secs(10),
    )
    .await;
    note(
        6,
        format!("box-b still receives hub traffic (after {attempts} attempt(s))"),
    );
    rig.hub
        .print_log(6, &["evoked", "Authorization", "authorization"]);
    rig.a
        .print_log(6, &["evoked", "Authorization", "authorization"]);
    note(
        6,
        "RESULT: PASS - account-JWT revocation closes the revoked user's connection (client or leaf) and refuses its reconnect; other users in the account are unaffected",
    );
}
