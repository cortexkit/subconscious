//! A real `nats-server` in operator mode whose operator and account JWTs the harness
//! wrote from fixture keys, so a user JWT ck-bus built is judged by the server itself.
//!
//! The box account names the harness signer's account root in its `signing_keys`, the
//! way the credential design has a vault root sign user JWTs for the account. The
//! operator and account identity keys are throwaway keys this module generates per run.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use nkeys::KeyPair;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    process::{Child, Command},
};

/// The foundation's minimum server version.
const MIN_VERSION: (u64, u64) = (2, 10);

/// Finds `nats-server` the way the foundation says (`CK_NATS_SERVER_BIN`, else `PATH`),
/// or returns the universal condition name and what was observed.
pub fn nats_server_bin() -> Result<(PathBuf, String), (&'static str, String)> {
    let path = match std::env::var_os("CK_NATS_SERVER_BIN") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join("nats-server"))
            .find(|candidate| candidate.is_file())
            .ok_or((
                "nats-server-absent",
                "no CK_NATS_SERVER_BIN and no nats-server on PATH".to_string(),
            ))?,
    };
    let output = std::process::Command::new(&path)
        .arg("--version")
        .output()
        .map_err(|error| ("nats-server-absent", format!("{}: {error}", path.display())))?;
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let numbers: Vec<u64> = version
        .rsplit('v')
        .next()
        .unwrap_or_default()
        .split('.')
        .filter_map(|part| part.parse().ok())
        .collect();
    match numbers.as_slice() {
        [major, minor, ..] if (*major, *minor) >= MIN_VERSION => Ok((path, version)),
        _ => Err(("nats-server-too-old", version)),
    }
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs() as i64
}

/// A nats-io/jwt v2 token signed by `signer` directly. Used only for the operator, the
/// account and the system user, which the harness writes itself.
fn encode_jwt(signer: &KeyPair, subject: &str, name: &str, nats: Value) -> String {
    let mut claims = json!({
        "jti": "",
        "iat": unix_now() - 120,
        "iss": signer.public_key(),
        "name": name,
        "sub": subject,
        "nats": nats,
    });
    let digest = Sha256::digest(claims.to_string().as_bytes());
    claims["jti"] = Value::String(super::hex(&digest).to_uppercase());
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

fn account_jwt(
    operator: &KeyPair,
    account: &KeyPair,
    name: &str,
    signing_keys: &[String],
) -> String {
    // Every limit is explicit: a missing limit decodes as 0, which the server enforces as
    // "none allowed".
    let mut nats = json!({
        "type": "account",
        "version": 2,
        "limits": {
            "subs": -1, "data": -1, "payload": -1, "imports": -1, "exports": -1,
            "wildcards": true, "conn": -1, "leaf": -1,
        },
        "default_permissions": {"pub": {}, "sub": {}},
    });
    if !signing_keys.is_empty() {
        nats["signing_keys"] = json!(signing_keys);
    }
    encode_jwt(operator, &account.public_key(), name, nats)
}

pub struct NatsFixture {
    pub dir: PathBuf,
    pub log: PathBuf,
    pub url: String,
    pub version: String,
    /// The box account's identity key (`A...`), the JWTs' `issuer_account`.
    pub account_public: String,
    /// Every key the harness wrote into the server config as a user-JWT issuer.
    pub trusted_issuers: BTreeSet<String>,
    child: Child,
}

impl NatsFixture {
    /// Writes the operator, system account and box account JWTs and starts the server.
    /// `account_signing_key` is the harness signer root (`A...`) the account trusts.
    pub async fn start(bin: &Path, version: String, dir: &Path, account_signing_key: &str) -> Self {
        std::fs::create_dir_all(dir).expect("nats fixture dir");
        let operator = KeyPair::new_operator();
        let system = KeyPair::new_account();
        let account = KeyPair::new_account();
        let operator_jwt = encode_jwt(
            &operator,
            &operator.public_key(),
            "ckbus-harness-operator",
            json!({"type": "operator", "version": 2, "system_account": system.public_key()}),
        );
        let operator_path = dir.join("operator.jwt");
        std::fs::write(&operator_path, operator_jwt).expect("operator jwt");
        let client_port = free_port();
        let http_port = free_port();
        let log = dir.join("server.log");
        let conf = format!(
            "listen: \"127.0.0.1:{client_port}\"\n\
             http: \"127.0.0.1:{http_port}\"\n\
             log_file: \"{log}\"\n\
             operator: \"{operator}\"\n\
             system_account: \"{system}\"\n\
             resolver: MEMORY\n\
             resolver_preload {{\n  {system}: \"{system_jwt}\"\n  {account}: \"{account_jwt}\"\n}}\n",
            log = log.display(),
            operator = operator_path.display(),
            system = system.public_key(),
            system_jwt = account_jwt(&operator, &system, "ckbus-harness-sys", &[]),
            account = account.public_key(),
            account_jwt = account_jwt(
                &operator,
                &account,
                "ckbus-harness-box",
                &[account_signing_key.to_string()]
            ),
        );
        let conf_path = dir.join("server.conf");
        std::fs::write(&conf_path, conf).expect("server conf");
        let child = Command::new(bin)
            .arg("-c")
            .arg(&conf_path)
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("XDG_RUNTIME_DIR", dir.join("run"))
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn nats-server");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !healthz(http_port).await {
            assert!(
                Instant::now() < deadline,
                "nats-server never answered /healthz; log at {}",
                log.display()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Self {
            dir: dir.to_path_buf(),
            log,
            url: format!("nats://127.0.0.1:{client_port}"),
            version,
            account_public: account.public_key(),
            trusted_issuers: BTreeSet::from([account_signing_key.to_string()]),
            child,
        }
    }

    /// Fails the run unless `jwt` is signed, under its own `iss`, by an issuer the harness
    /// wrote into this server's config. An arm that authenticates with anything else
    /// would prove nothing about the signer.
    pub fn assert_harness_signed(&self, jwt: &str) {
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "a JWT has three parts");
        let claims: Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(parts[1])
                .expect("JWT claims are base64url"),
        )
        .expect("JWT claims are JSON");
        let issuer = claims["iss"].as_str().expect("JWT names its issuer");
        assert!(
            self.trusted_issuers.contains(issuer),
            "JWT issuer {issuer} is not a key the harness wrote into the server config"
        );
        let signature = URL_SAFE_NO_PAD
            .decode(parts[2])
            .expect("JWT signature is base64url");
        KeyPair::from_public_key(issuer)
            .expect("issuer is an nkey")
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .expect("the JWT signature must verify under its harness-written issuer");
    }

    pub fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    pub async fn stop(mut self) {
        let _ = self.child.kill().await;
    }
}

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
