//! The install-time trust chain and a local `nats-server` configured the way `ck setup`
//! configures it (`docs/designs/nats-install-trust-chain.md`, sections 1 and 4), built
//! from throwaway fixture keys, plus the observers the bootstrap rows read.
//!
//! - The operator ROOT self-signs the operator JWT (`signing_keys` = the operator signer)
//!   and signs the system account JWT (`signing_keys` = the system account key). The
//!   harness holds it the way the install ceremony would; ck-bus never sees it.
//! - The harness signer holds the three keys ck-bus signs with, under their production
//!   credential ids and with throwaway key material: the operator signer, the system
//!   account key and the box account key.
//! - The server runs the full (directory) resolver with deletion disabled, preloads only
//!   the system account, and listens on loopback only, with no TLS.
//!
//! No box account exists before ck-bus boots: ck-bus creates it.
#![allow(dead_code)]

use std::{
    collections::BTreeSet,
    num::NonZeroU32,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use cortexkit_bus_naming::{root_credential_id, RootCredentialKind};
use nkeys::KeyPair;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subc_control::{ClientControlRequest, ClientControlResponse};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    process::{Child, Command},
};

use super::{
    control,
    signer::{nats::unix_now, HarnessSigner},
};

/// The credential id the operator minted `signing:<provider>:1` under.
pub fn root_id(provider: &str) -> String {
    root_credential_id(RootCredentialKind::Signing, provider, NonZeroU32::new(1))
        .expect("a fixed provider token is in the lexicon")
}

pub fn box_root_id() -> String {
    root_id("ck-bus-account")
}

pub fn system_root_id() -> String {
    root_id("ck-bus-sysaccount")
}

pub fn signer_root_id() -> String {
    root_id("ck-bus-operator-signer")
}

fn encode_jwt(signer: &KeyPair, subject: &str, name: &str, iat: i64, nats: Value) -> String {
    let mut claims = json!({
        "jti": "",
        "iat": iat,
        "iss": signer.public_key(),
        "name": name,
        "sub": subject,
        "nats": nats,
    });
    let digest = Sha256::digest(claims.to_string().as_bytes());
    claims["jti"] = Value::String(super::signer::hex(&digest).to_uppercase());
    let input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(br#"{"alg":"ed25519-nkey","typ":"JWT"}"#),
        URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes())
    );
    let signature = signer
        .sign(input.as_bytes())
        .expect("fixture keys hold seeds");
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
}

/// A decoded JWT's claims.
pub fn claims(jwt: &str) -> Value {
    let part = jwt.trim().split('.').nth(1).expect("a JWT has claims");
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).expect("claims are base64url"))
        .expect("claims are JSON")
}

/// The fixture trust chain.
pub struct TrustChain {
    pub signer: HarnessSigner,
    operator_root: KeyPair,
    /// The system account id. Its identity seed was dropped at generation, as setup
    /// does.
    pub system_account: String,
}

impl TrustChain {
    pub fn generate() -> Self {
        let signer = HarnessSigner::from_pairs([
            (signer_root_id(), KeyPair::new_operator()),
            (system_root_id(), KeyPair::new_account()),
            (box_root_id(), KeyPair::new_account()),
        ]);
        Self {
            signer,
            operator_root: KeyPair::new_operator(),
            system_account: KeyPair::new_account().public_key(),
        }
    }

    /// The operator signer as the operator JWT lists it (`O...`).
    pub fn signer_public(&self) -> String {
        self.signer.root(&signer_root_id()).account_public()
    }

    /// The box account root as a box account JWT lists it (`A...`).
    pub fn box_root_public(&self) -> String {
        self.signer.root(&box_root_id()).account_public()
    }

    pub fn operator_jwt(&self) -> String {
        encode_jwt(
            &self.operator_root,
            &self.operator_root.public_key(),
            "ckbus-harness-operator",
            unix_now() - 120,
            json!({
                "type": "operator",
                "version": 2,
                "system_account": self.system_account,
                "signing_keys": [self.signer_public()],
                "strict_signing_key_usage": false,
            }),
        )
    }

    pub fn system_account_jwt(&self) -> String {
        encode_jwt(
            &self.operator_root,
            &self.system_account,
            "SYS",
            unix_now() - 120,
            json!({
                "type": "account",
                "version": 2,
                "limits": {
                    "subs": -1, "data": -1, "payload": -1, "imports": -1, "exports": -1,
                    "wildcards": true, "conn": -1, "leaf": -1,
                },
                "signing_keys": [self.signer.root(&system_root_id()).account_public()],
                "default_permissions": {"pub": {}, "sub": {}},
            }),
        )
    }

    /// A harness user in `account_public`, signed by the root the account lists, with no
    /// permission restriction.
    pub fn user_jwt(
        &self,
        root_id: &str,
        account_public: &str,
        user: &KeyPair,
        iat: i64,
    ) -> String {
        self.user_jwt_for_subject(root_id, account_public, &user.public_key(), iat)
    }

    /// `user_jwt` for a user key given by its public half only.
    pub fn user_jwt_for_subject(
        &self,
        root_id: &str,
        account_public: &str,
        subject: &str,
        iat: i64,
    ) -> String {
        encode_jwt(
            &self.signer.root(root_id).pair,
            subject,
            "harness-user",
            iat,
            json!({
                "type": "user", "version": 2, "issuer_account": account_public,
                "pub": {}, "sub": {}, "subs": -1, "data": -1, "payload": -1,
            }),
        )
    }
}

/// Connects with `jwt`, answering the nonce with `user`.
pub async fn connect(
    url: &str,
    jwt: String,
    user: KeyPair,
) -> Result<async_nats::Client, async_nats::ConnectError> {
    let user = std::sync::Arc::new(user);
    async_nats::ConnectOptions::with_jwt(jwt, move |nonce| {
        let user = user.clone();
        async move { user.sign(&nonce).map_err(async_nats::AuthError::new) }
    })
    .connection_timeout(Duration::from_secs(5))
    .connect(url)
    .await
}

/// A harness client in the box account.
pub async fn box_client(
    trust: &TrustChain,
    server: &BusServer,
    account_public: &str,
) -> async_nats::Client {
    let user = KeyPair::new_user();
    let jwt = trust.user_jwt(&box_root_id(), account_public, &user, unix_now() - 60);
    connect(&server.url, jwt, user)
        .await
        .unwrap_or_else(|error| {
            panic!("harness box client refused: {error}\n{}", server.log_text())
        })
}

/// A harness client in the system account.
pub async fn system_client(trust: &TrustChain, server: &BusServer) -> async_nats::Client {
    let user = KeyPair::new_user();
    let jwt = trust.user_jwt(
        &system_root_id(),
        &trust.system_account,
        &user,
        unix_now() - 60,
    );
    connect(&server.url, jwt, user)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "harness system client refused: {error}\n{}",
                server.log_text()
            )
        })
}

/// The stored account JWT for `account_public`, read through the claims lookup.
pub async fn lookup(system: &async_nats::Client, account_public: &str) -> Option<String> {
    let reply = system
        .request(
            format!("$SYS.REQ.ACCOUNT.{account_public}.CLAIMS.LOOKUP"),
            Vec::new().into(),
        )
        .await
        .expect("claims lookup answers");
    (!reply.payload.is_empty()).then(|| String::from_utf8(reply.payload.to_vec()).unwrap())
}

pub struct BusServer {
    pub dir: PathBuf,
    pub url: String,
    pub port: u16,
    pub listen_host: String,
    pub log: PathBuf,
    pub operator_jwt: PathBuf,
    pub system_account: String,
    conf: PathBuf,
    bin: PathBuf,
    http_port: u16,
    child: Option<Child>,
}

impl BusServer {
    /// Writes setup's outputs and starts the server listening on `listen_host`. Every
    /// production-shaped caller passes `LOOPBACK`.
    pub async fn start(bin: &Path, dir: &Path, trust: &TrustChain, listen_host: &str) -> Self {
        std::fs::create_dir_all(dir.join("jwt")).expect("resolver dir");
        let operator_jwt = dir.join("operator.jwt");
        std::fs::write(&operator_jwt, trust.operator_jwt()).expect("operator jwt");
        let port = free_port();
        let http_port = free_port();
        let log = dir.join("server.log");
        // Stock nats-server has one client listener, so the harness binds the IPv4
        // loopback address explicitly; it never binds the wildcard address.
        // `max_control_line` is raised because a user JWT carrying ck-bus's box grant
        // makes a CONNECT line longer than the server's 4 KiB default, which the server
        // refuses as "maximum control line exceeded". `debug` makes the log name why a
        // connect was refused (revoked versus a bad signature) and why a client closed.
        // The debug log shows public keys and JWTs, never a seed.
        let conf = format!(
            "listen: \"{listen_host}:{port}\"\n\
             max_control_line: 65536\n\
             debug: true\n\
             http: \"127.0.0.1:{http_port}\"\n\
             log_file: \"{log}\"\n\
             jetstream {{ store_dir: \"{js}\" }}\n\
             operator: \"{operator}\"\n\
             system_account: \"{system}\"\n\
             resolver {{ type: full, dir: \"{jwt}\", allow_delete: false, interval: \"2m\" }}\n\
             resolver_preload {{\n  {system}: \"{system_jwt}\"\n}}\n",
            log = log.display(),
            js = dir.join("js").display(),
            operator = operator_jwt.display(),
            system = trust.system_account,
            jwt = dir.join("jwt").display(),
            system_jwt = trust.system_account_jwt(),
        );
        let conf_path = dir.join("server.conf");
        std::fs::write(&conf_path, conf).expect("server conf");
        let mut server = Self {
            dir: dir.to_path_buf(),
            url: format!("nats://127.0.0.1:{port}"),
            port,
            listen_host: listen_host.to_string(),
            log,
            operator_jwt,
            system_account: trust.system_account.clone(),
            conf: conf_path,
            bin: bin.to_path_buf(),
            http_port,
            child: None,
        };
        server.spawn().await;
        server
    }

    async fn spawn(&mut self) {
        let child = Command::new(&self.bin)
            .arg("-c")
            .arg(&self.conf)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn nats-server");
        self.child = Some(child);
        let deadline = Instant::now() + Duration::from_secs(20);
        while !healthz(self.http_port).await {
            assert!(
                Instant::now() < deadline,
                "nats-server never answered /healthz; log at {}",
                self.log.display()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// ck-bus's broker inputs, as its supervised environment carries them.
    pub fn ckbus_env(&self) -> Vec<(String, String)> {
        vec![
            ("CKBUS_NATS_URL".to_string(), self.url.clone()),
            (
                "CKBUS_OPERATOR_JWT".to_string(),
                self.operator_jwt.display().to_string(),
            ),
            (
                "CKBUS_SYSTEM_ACCOUNT".to_string(),
                self.system_account.clone(),
            ),
        ]
    }

    /// Every account id the directory resolver has stored (one `<id>.jwt` each).
    pub fn stored_accounts(&self) -> BTreeSet<String> {
        std::fs::read_dir(self.dir.join("jwt"))
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter_map(|entry| {
                        entry
                            .file_name()
                            .to_str()
                            .and_then(|name| name.strip_suffix(".jwt"))
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    pub async fn stop(mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
        }
    }
}

pub const LOOPBACK: &str = "127.0.0.1";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind probe port")
        .local_addr()
        .expect("probe addr")
        .port()
}

async fn healthz(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)).await else {
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
    let read = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut buf)).await;
    matches!(read, Ok(Ok(_))) && String::from_utf8_lossy(&buf).contains("\"ok\"")
}

/// Every structured ck-bus log line named `event` in the run's capture logs, oldest
/// first within each file.
pub fn events(run_root: &Path, event: &str) -> Vec<Value> {
    let mut found = Vec::new();
    let mut stack = vec![run_root.join("run/logs")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut paths: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            for line in text.lines() {
                let Some(start) = line.find('{') else {
                    continue;
                };
                if let Ok(value) = serde_json::from_str::<Value>(&line[start..]) {
                    if value["event"] == event {
                        found.push(value);
                    }
                }
            }
        }
    }
    found
}

/// Waits until at least `count` `event` lines exist and returns the last one.
pub async fn wait_event(run_root: &Path, event: &str, count: usize, limit: Duration) -> Value {
    let deadline = Instant::now() + limit;
    loop {
        let found = events(run_root, event);
        if found.len() >= count {
            return found.last().cloned().expect("at least one event");
        }
        assert!(
            Instant::now() < deadline,
            "no {count}th {event} line within {limit:?}; last bootstrap.down line: {:?}",
            events(run_root, "ckbus.bootstrap.down").last()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `supervisor.health_probe` for `ckbus`: status, detail and metrics.
pub async fn health(connection_file: &Path) -> (String, Option<String>, Value) {
    try_health(connection_file)
        .await
        .unwrap_or_else(|refusal| panic!("supervisor.health_probe ckbus refused: {refusal}"))
}

/// `health`, with a refused probe (the module is between processes) as an error.
pub async fn try_health(connection_file: &Path) -> Result<(String, Option<String>, Value), String> {
    let response = match control::rpc(
        connection_file,
        ClientControlRequest::SupervisorHealthProbe {
            module_id: "ckbus".to_string(),
        },
    )
    .await
    {
        control::ControlReply::Response(response) => response,
        control::ControlReply::Error(error) => {
            return Err(format!("{} {}", error.code, error.message))
        }
    };
    let ClientControlResponse::SupervisorHealthProbe {
        status,
        detail,
        metrics,
        ..
    } = response
    else {
        panic!("supervisor.health_probe must return its matching response variant");
    };
    Ok((
        format!("{status:?}"),
        detail,
        metrics.unwrap_or(Value::Null),
    ))
}

/// Waits until the health probe's `metrics.cause` equals `cause`.
pub async fn wait_health_cause(connection_file: &Path, cause: &str, limit: Duration) -> Value {
    let deadline = Instant::now() + limit;
    loop {
        let last = try_health(connection_file).await;
        if let Ok((_, _, metrics)) = &last {
            if metrics["cause"] == cause {
                return metrics.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "health never named {cause} within {limit:?}; last probe {last:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A real supervised restart of `ckbus` through `supervisor.restart`.
pub async fn restart_ckbus(connection_file: &Path) {
    let reply = control::rpc(
        connection_file,
        ClientControlRequest::SupervisorRestart {
            module_id: "ckbus".to_string(),
            drain_timeout_ms: None,
        },
    )
    .await;
    if let control::ControlReply::Error(error) = reply {
        panic!(
            "supervisor.restart ckbus refused: {} {}",
            error.code, error.message
        );
    }
}

/// `account.json` in the run's ckbus store.
pub fn account_json(run_root: &Path) -> PathBuf {
    run_root.join("data/cortexkit/ckbus/account.json")
}

pub fn own_users_json(run_root: &Path) -> PathBuf {
    run_root.join("data/cortexkit/ckbus/own_users.json")
}

pub fn read_json(path: &Path) -> Value {
    serde_json::from_slice(
        &std::fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
    )
    .expect("store file is JSON")
}
