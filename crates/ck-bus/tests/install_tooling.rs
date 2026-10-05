//! `ck-bus install-plan` and `ck-bus install-apply`, driven as `ck setup` drives them:
//! the built binary, offline (no `SUBC_MODULE_ID`, no daemon, no vault), with the root
//! ceremony played by a fixture operator key the test holds.
//!
//! The server arm starts a real `nats-server` from the written config. It is skipped,
//! with a loud `SKIP` line, only when no `nats-server` is found (`CK_NATS_SERVER_BIN`,
//! else `PATH`).

#[allow(dead_code)]
#[path = "harness/signer/seeds.rs"]
mod seeds;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use nkeys::KeyPair;
use serde_json::{json, Value};

const HEADER: &str = r#"{"alg":"ed25519-nkey","typ":"JWT"}"#;

/// The fixture keys the ceremony would use: the operator root, the operator signer and
/// the system account signing key.
struct Fixture {
    root: KeyPair,
    signer: KeyPair,
    sysaccount: KeyPair,
    dir: tempfile::TempDir,
}

fn raw_hex(pair: &KeyPair) -> String {
    let (_, raw) = nkeys::from_public_key(&pair.public_key()).expect("a public nkey");
    raw.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

struct Output {
    ok: bool,
    json: Value,
    stderr: String,
}

fn ck_bus(args: &[&str]) -> Output {
    let homes = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ck-bus"))
        .args(args)
        .env_remove("SUBC_MODULE_ID")
        .env("XDG_DATA_HOME", homes.path().join("data"))
        .env("XDG_RUNTIME_DIR", homes.path().join("run"))
        .env("XDG_CONFIG_HOME", homes.path().join("config"))
        .stdin(Stdio::null())
        .output()
        .expect("run ck-bus");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    Output {
        ok: output.status.success(),
        json: serde_json::from_str(&stdout).unwrap_or(Value::Null),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

/// Every file under `root`, by path relative to it, with its bytes.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            files.insert(path.strip_prefix(root).unwrap().join(""), Vec::new());
            for entry in std::fs::read_dir(&path).unwrap() {
                pending.push(entry.unwrap().path());
            }
        } else {
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            files.insert(relative, std::fs::read(&path).unwrap());
        }
    }
    files
}

/// Atomic rewrites can preserve bytes while still touching a JWT. Record other files'
/// mtimes as well, so the monitoring-only arm proves it leaves them alone.
fn other_file_mtimes(root: &Path) -> BTreeMap<PathBuf, SystemTime> {
    snapshot(root)
        .into_keys()
        .filter(|path| path != Path::new("nats/server.conf"))
        .filter_map(|path| {
            let metadata = std::fs::metadata(root.join(&path)).unwrap();
            metadata
                .is_file()
                .then(|| (path, metadata.modified().unwrap()))
        })
        .collect()
}

impl Fixture {
    fn new() -> Self {
        Self {
            root: KeyPair::new_operator(),
            signer: KeyPair::new_operator(),
            sysaccount: KeyPair::new_account(),
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn nats_dir(&self) -> PathBuf {
        self.dir.path().join("nats")
    }

    fn plan(&self) -> Output {
        let nats = self.nats_dir();
        ck_bus(&[
            "install-plan",
            "--nats-dir",
            nats.to_str().unwrap(),
            "--root-pub",
            &raw_hex(&self.root),
            "--signer-pub",
            &raw_hex(&self.signer),
            "--sysaccount-pub",
            &raw_hex(&self.sysaccount),
        ])
    }

    fn plan_ok(&self) -> Value {
        let output = self.plan();
        assert!(output.ok, "install-plan refused: {}", output.stderr);
        output.json
    }

    /// The root's signature over a planned payload file, as the ceremony returns it.
    fn sign_file(&self, signer: &KeyPair, path: &str) -> String {
        let input = std::fs::read(path).unwrap();
        URL_SAFE_NO_PAD.encode(signer.sign(&input).unwrap())
    }

    fn apply(&self, operator: (&str, &str), sysaccount: (&str, &str), port: u16) -> Output {
        self.apply_with_ports(operator, sysaccount, port, 18222)
    }

    fn apply_with_ports(
        &self,
        operator: (&str, &str),
        sysaccount: (&str, &str),
        port: u16,
        monitor_port: u16,
    ) -> Output {
        let nats = self.nats_dir();
        ck_bus(&[
            "install-apply",
            "--nats-dir",
            nats.to_str().unwrap(),
            "--root-pub",
            &raw_hex(&self.root),
            "--operator-input",
            operator.0,
            "--operator-sig",
            operator.1,
            "--sysaccount-input",
            sysaccount.0,
            "--sysaccount-sig",
            sysaccount.1,
            "--port",
            &port.to_string(),
            "--monitor-port",
            &monitor_port.to_string(),
        ])
    }

    /// Plans, has the root sign both payloads, and applies. Returns the apply output.
    fn install(&self, port: u16) -> Value {
        self.install_with_ports(port, 18222)
    }

    fn install_with_ports(&self, port: u16, monitor_port: u16) -> Value {
        let plan = self.plan_ok();
        let (operator, sysaccount) = payload_paths(&plan);
        let operator_sig = self.sign_file(&self.root, &operator);
        let sysaccount_sig = self.sign_file(&self.root, &sysaccount);
        let output = self.apply_with_ports(
            (&operator, &operator_sig),
            (&sysaccount, &sysaccount_sig),
            port,
            monitor_port,
        );
        assert!(output.ok, "install-apply refused: {}", output.stderr);
        output.json
    }
}

fn payload_path(plan: &Value, label: &str) -> Option<String> {
    plan["payloads"]
        .as_array()?
        .iter()
        .find(|payload| payload["payload"] == label)
        .map(|payload| payload["path"].as_str().unwrap().to_string())
}

fn payload_paths(plan: &Value) -> (String, String) {
    (
        payload_path(plan, "operator").expect("an operator payload"),
        payload_path(plan, "system_account").expect("a system account payload"),
    )
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn nats_server_bin() -> Option<PathBuf> {
    match std::env::var_os("CK_NATS_SERVER_BIN") {
        Some(path) if !path.is_empty() => Some(PathBuf::from(path)),
        _ => std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join(format!("nats-server{}", std::env::consts::EXE_SUFFIX)))
            .find(|candidate| candidate.is_file()),
    }
}

/// A user JWT in the system account, signed by the system account signing key with
/// `issuer_account` set, the shape ck-bus issues its system user in.
fn system_user_jwt(sysaccount: &KeyPair, system_account: &str, user: &KeyPair) -> String {
    let claims = json!({
        "jti": format!("TEST{}", user.public_key()),
        "iat": unix_now() - 60,
        "iss": sysaccount.public_key(),
        "name": "install-tooling-sys-user",
        "sub": user.public_key(),
        "nats": {
            "type": "user", "version": 2, "issuer_account": system_account,
            "pub": {}, "sub": {}, "subs": -1, "data": -1, "payload": -1,
        },
    });
    let input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(HEADER),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let signature = sysaccount.sign(input.as_bytes()).unwrap();
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
}

async fn connect_system_user(
    url: &str,
    jwt: String,
    user: KeyPair,
) -> Result<async_nats::Client, async_nats::ConnectError> {
    let user = std::sync::Arc::new(user);
    async_nats::ConnectOptions::with_jwt(jwt, move |nonce| {
        let user = user.clone();
        async move { user.sign(&nonce).map_err(async_nats::AuthError::new) }
    })
    .connection_timeout(Duration::from_secs(2))
    .connect(url)
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn planned_and_applied_config_starts_nats_and_a_system_user_reaches_sys() {
    let Some(bin) = nats_server_bin() else {
        eprintln!("SKIP install tooling server arm: nats-server-absent: no CK_NATS_SERVER_BIN and no nats-server on PATH");
        return;
    };
    let fixture = Fixture::new();
    // Hold both allocations together so the client and monitor ports cannot coincide.
    let client_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let monitor_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = client_listener.local_addr().unwrap().port();
    let monitor_port = monitor_listener.local_addr().unwrap().port();
    drop((client_listener, monitor_listener));
    let applied = fixture.install_with_ports(port, monitor_port);
    let env = &applied["env"];
    let url = env["CKBUS_NATS_URL"].as_str().unwrap();
    let system_account = env["CKBUS_SYSTEM_ACCOUNT"].as_str().unwrap().to_string();
    assert_eq!(url, format!("nats://127.0.0.1:{port}"));
    assert_eq!(
        applied["health_url"],
        format!("http://127.0.0.1:{monitor_port}/healthz")
    );
    assert_eq!(
        env["CKBUS_OPERATOR_JWT"],
        fixture
            .nats_dir()
            .join("operator.jwt")
            .display()
            .to_string()
    );

    let log = fixture.dir.path().join("server.log");
    let mut child = tokio::process::Command::new(&bin)
        .arg("-c")
        .arg(applied["server_conf"].as_str().unwrap())
        .env("XDG_DATA_HOME", fixture.dir.path().join("data"))
        .env("XDG_RUNTIME_DIR", fixture.dir.path().join("run"))
        .env("XDG_CONFIG_HOME", fixture.dir.path().join("config"))
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn nats-server");

    let deadline = Instant::now() + Duration::from_secs(20);
    let client = loop {
        let user = KeyPair::new_user();
        let jwt = system_user_jwt(&fixture.sysaccount, &system_account, &user);
        match connect_system_user(url, jwt, user).await {
            Ok(client) => break client,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "the system user never connected: {error}\n{}",
                    std::fs::read_to_string(&log).unwrap_or_default()
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };
    // Probe the reported plain URL, not a synthesized readiness substitute.
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let health_url = applied["health_url"].as_str().unwrap();
    let (authority, path) = health_url
        .strip_prefix("http://")
        .unwrap()
        .split_once('/')
        .unwrap();
    let mut http = tokio::net::TcpStream::connect(authority).await.unwrap();
    http.write_all(format!("GET /{path} HTTP/1.0\r\nHost: {authority}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut response = String::new();
    tokio::time::timeout(Duration::from_secs(2), http.read_to_string(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(
        response.starts_with("HTTP/1.0 200") || response.starts_with("HTTP/1.1 200"),
        "{response}"
    );
    // Only a system account user is served the claims lookup, and the answer is the
    // root-signed system account JWT the server stored from the preload.
    let reply = client
        .request(
            format!("$SYS.REQ.ACCOUNT.{system_account}.CLAIMS.LOOKUP"),
            Vec::new().into(),
        )
        .await
        .expect("claims lookup answers");
    let stored = String::from_utf8(reply.payload.to_vec()).unwrap();
    let claims: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(stored.split('.').nth(1).unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(claims["sub"], system_account);
    assert_eq!(claims["iss"], fixture.root.public_key());

    // A user signed by a key the system account does not list is refused, so the
    // connect above proves the written trust chain rather than an open server.
    let stranger = KeyPair::new_account();
    let user = KeyPair::new_user();
    let jwt = system_user_jwt(&stranger, &system_account, &user);
    assert!(connect_system_user(url, jwt, user).await.is_err());

    drop(client);
    let _ = child.kill().await;

    // Nothing under the nats dir holds a seed, including what the server itself wrote
    // (the stored preload under jwt/).
    let found = seeds::scan_tree(&fixture.nats_dir());
    assert!(found.is_empty(), "seeds found: {found:?}");
    assert!(fixture.nats_dir().join("jwt").read_dir().unwrap().count() > 0);
    // The scan sees a seed planted in a copy of the written config.
    let planted = fixture.nats_dir().join("planted.conf");
    let conf = std::fs::read_to_string(fixture.nats_dir().join("server.conf")).unwrap();
    std::fs::write(
        &planted,
        format!("{conf}# {}\n", KeyPair::new_user().seed().unwrap()),
    )
    .unwrap();
    assert_eq!(seeds::scan_tree(&fixture.nats_dir()).len(), 1);
}

#[test]
fn apply_refuses_a_signature_over_other_bytes_or_by_another_key_and_writes_nothing() {
    let fixture = Fixture::new();
    let plan = fixture.plan_ok();
    let (operator, sysaccount) = payload_paths(&plan);
    let good_operator = fixture.sign_file(&fixture.root, &operator);
    let good_sysaccount = fixture.sign_file(&fixture.root, &sysaccount);
    let before = snapshot(&fixture.nats_dir());

    // The root's signature over the other payload's bytes.
    let other_bytes = fixture.sign_file(&fixture.root, &sysaccount);
    // The right bytes, signed by the operator signer rather than the pinned root.
    let other_key = fixture.sign_file(&fixture.signer, &operator);
    for (label, operator_sig, sysaccount_sig) in [
        ("operator over other bytes", &other_bytes, &good_sysaccount),
        ("operator by another key", &other_key, &good_sysaccount),
        (
            "system account by another key",
            &good_operator,
            &fixture.sign_file(&fixture.signer, &sysaccount),
        ),
    ] {
        let output = fixture.apply(
            (&operator, operator_sig),
            (&sysaccount, sysaccount_sig),
            free_port(),
        );
        assert!(!output.ok, "{label}: apply must refuse");
        assert!(
            output.stderr.contains("does not verify"),
            "{label}: {}",
            output.stderr
        );
        assert_eq!(
            snapshot(&fixture.nats_dir()),
            before,
            "{label}: apply wrote"
        );
    }
    // The control: the same invocation with both good signatures applies.
    let output = fixture.apply(
        (&operator, &good_operator),
        (&sysaccount, &good_sysaccount),
        free_port(),
    );
    assert!(output.ok, "{}", output.stderr);
}

#[test]
fn a_second_plan_after_apply_needs_no_ceremony_and_changes_no_byte() {
    let fixture = Fixture::new();
    let port = free_port();
    fixture.install(port);
    let before = snapshot(fixture.dir.path());
    let conf = std::fs::read_to_string(fixture.nats_dir().join("server.conf")).unwrap();
    assert!(
        conf.contains(&format!("listen: \"127.0.0.1:{port}\"\n")),
        "{conf}"
    );
    assert_eq!(conf.matches("listen").count(), 1, "{conf}");

    // A later second, so a plan that re-signed or rewrote a payload "to refresh it"
    // would change its `iat`, and with it the bytes.
    std::thread::sleep(Duration::from_millis(1100));
    let again = fixture.plan_ok();
    assert_eq!(again["status"], "no ceremony needed", "{again}");
    assert!(again.get("payloads").is_none());
    assert_eq!(snapshot(fixture.dir.path()), before);
}

#[test]
fn the_system_account_id_is_never_regenerated() {
    let fixture = Fixture::new();
    let first = fixture.plan_ok();
    let id = first["system_account"].as_str().unwrap().to_string();
    assert!(id.starts_with('A'));
    let file = fixture.nats_dir().join("system_account");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), format!("{id}\n"));

    // A second plan before any ceremony still needs one, over the same account.
    let second = fixture.plan_ok();
    assert_eq!(second["status"], "ceremony needed");
    assert_eq!(second["system_account"], id);
    assert_eq!(
        second["payloads"][0]["claims"]["system_account"], id,
        "{second}"
    );
    assert_eq!(second["payloads"][1]["claims"]["sub"], id, "{second}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), format!("{id}\n"));
}

#[test]
fn a_rotated_sysaccount_key_needs_only_the_system_account_payload() {
    let mut fixture = Fixture::new();
    fixture.install(free_port());
    let operator_before = std::fs::read(fixture.nats_dir().join("operator.jwt")).unwrap();
    fixture.sysaccount = KeyPair::new_account();

    let plan = fixture.plan_ok();
    assert_eq!(plan["status"], "ceremony needed");
    assert_eq!(payload_path(&plan, "operator"), None, "{plan}");
    let sysaccount = payload_path(&plan, "system_account").expect("the system account payload");
    let reason = plan["payloads"][0]["reason"].as_str().unwrap();
    assert!(reason.contains("nats.signing_keys"), "{reason}");

    let nats = fixture.nats_dir();
    let signature = fixture.sign_file(&fixture.root, &sysaccount);
    let output = ck_bus(&[
        "install-apply",
        "--nats-dir",
        nats.to_str().unwrap(),
        "--root-pub",
        &raw_hex(&fixture.root),
        "--sysaccount-input",
        &sysaccount,
        "--sysaccount-sig",
        &signature,
    ]);
    assert!(output.ok, "{}", output.stderr);
    // The unchanged operator JWT is kept, never re-signed.
    assert_eq!(
        std::fs::read(nats.join("operator.jwt")).unwrap(),
        operator_before
    );
    assert_eq!(fixture.plan_ok()["status"], "no ceremony needed");
}

#[test]
fn plan_prints_the_file_hash_and_decoded_claims_of_each_input() {
    let fixture = Fixture::new();
    let plan = fixture.plan_ok();
    for payload in plan["payloads"].as_array().unwrap() {
        let path = payload["path"].as_str().unwrap();
        let bytes = std::fs::read(path).unwrap();
        let digest: String =
            sha2::Digest::finalize(<sha2::Sha256 as sha2::Digest>::new_with_prefix(&bytes))
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
        assert_eq!(payload["sha256"], digest);
        let claims: Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(
                    std::str::from_utf8(&bytes)
                        .unwrap()
                        .split('.')
                        .nth(1)
                        .unwrap(),
                )
                .unwrap(),
        )
        .unwrap();
        for field in ["iss", "sub", "name", "iat", "jti"] {
            assert_eq!(payload["claims"][field], claims[field], "{field}");
        }
        assert_eq!(
            payload["claims"]["signing_keys"],
            claims["nats"]["signing_keys"]
        );
        assert_eq!(claims["iss"], fixture.root.public_key());
    }
    let operator = &plan["payloads"][0]["claims"];
    assert_eq!(operator["sub"], fixture.root.public_key());
    assert_eq!(
        operator["signing_keys"],
        json!([fixture.signer.public_key()])
    );
    assert_eq!(operator["system_account"], plan["system_account"]);
    let system = &plan["payloads"][1]["claims"];
    assert_eq!(system["sub"], plan["system_account"]);
    assert_eq!(
        system["signing_keys"],
        json!([fixture.sysaccount.public_key()])
    );
}

#[test]
fn apply_refuses_a_root_signed_payload_carrying_a_seed_and_writes_nothing() {
    let fixture = Fixture::new();
    let plan = fixture.plan_ok();
    let (operator, sysaccount) = payload_paths(&plan);
    // A canonical, root-signed operator input whose name is an nkey seed: every check
    // but the seed scan accepts it.
    let input = std::fs::read_to_string(&operator).unwrap();
    let mut claims: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(input.split('.').nth(1).unwrap())
            .unwrap(),
    )
    .unwrap();
    claims["name"] = Value::String(KeyPair::new_account().seed().unwrap());
    let planted = fixture.dir.path().join("planted.input");
    std::fs::write(
        &planted,
        format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(HEADER),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        ),
    )
    .unwrap();
    let planted = planted.to_str().unwrap().to_string();
    let before = snapshot(&fixture.nats_dir());
    let output = fixture.apply(
        (&planted, &fixture.sign_file(&fixture.root, &planted)),
        (&sysaccount, &fixture.sign_file(&fixture.root, &sysaccount)),
        free_port(),
    );
    assert!(!output.ok, "apply must refuse a seed");
    assert!(output.stderr.contains("nkey seed"), "{}", output.stderr);
    assert_eq!(snapshot(&fixture.nats_dir()), before);
}

#[test]
fn install_commands_run_without_a_module_id_and_refuse_bad_keys() {
    let fixture = Fixture::new();
    let nats = fixture.nats_dir();
    let output = ck_bus(&[
        "install-plan",
        "--nats-dir",
        nats.to_str().unwrap(),
        "--root-pub",
        "abcd",
        "--signer-pub",
        &raw_hex(&fixture.signer),
        "--sysaccount-pub",
        &raw_hex(&fixture.sysaccount),
    ]);
    assert!(!output.ok);
    assert!(output.stderr.contains("--root-pub"), "{}", output.stderr);
    assert!(!nats.exists(), "a refused plan creates nothing");
}

#[test]
fn apply_reports_the_chosen_monitoring_health_url_and_defaults() {
    let fixture = Fixture::new();
    let applied = fixture.install_with_ports(14223, 19223);
    assert_eq!(applied["health_url"], "http://127.0.0.1:19223/healthz");
    let conf = std::fs::read_to_string(fixture.nats_dir().join("server.conf")).unwrap();
    assert!(conf.contains("http: \"127.0.0.1:19223\"\n"), "{conf}");

    // Without either flag, normal apply follows the client and monitor defaults.
    let nats = fixture.nats_dir();
    let output = ck_bus(&[
        "install-apply",
        "--nats-dir",
        nats.to_str().unwrap(),
        "--root-pub",
        &raw_hex(&fixture.root),
    ]);
    assert!(output.ok, "{}", output.stderr);
    assert_eq!(
        output.json["env"]["CKBUS_NATS_URL"],
        "nats://127.0.0.1:14222"
    );
    assert_eq!(output.json["health_url"], "http://127.0.0.1:18222/healthz");
    let conf = std::fs::read_to_string(nats.join("server.conf")).unwrap();
    assert!(conf.contains("http: \"127.0.0.1:18222\"\n"), "{conf}");
}

#[test]
fn apply_refuses_equal_client_and_monitor_ports_and_writes_nothing() {
    let fixture = Fixture::new();
    let plan = fixture.plan_ok();
    let (operator, sysaccount) = payload_paths(&plan);
    let before = snapshot(fixture.dir.path());
    let output = fixture.apply_with_ports(
        (&operator, &fixture.sign_file(&fixture.root, &operator)),
        (&sysaccount, &fixture.sign_file(&fixture.root, &sysaccount)),
        14222,
        14222,
    );
    assert!(!output.ok);
    assert!(
        output.stderr.contains("monitor port must differ"),
        "{}",
        output.stderr
    );
    assert_eq!(snapshot(fixture.dir.path()), before);
}

fn conf_only(fixture: &Fixture, extra: &[&str]) -> Output {
    let nats = fixture.nats_dir();
    let mut args = vec![
        "install-apply",
        "--conf-only",
        "--nats-dir",
        nats.to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    ck_bus(&args)
}

#[test]
fn conf_only_adds_only_monitoring_and_matches_fresh_apply() {
    let fixture = Fixture::new();
    fixture.install_with_ports(15222, 18222);
    let nats = fixture.nats_dir();
    let conf_path = nats.join("server.conf");
    let fresh = std::fs::read_to_string(&conf_path).unwrap();
    let legacy = fresh.replace("http: \"127.0.0.1:18222\"\n", "");
    assert_ne!(legacy, fresh, "the legacy fixture lacks monitoring");
    std::fs::write(&conf_path, &legacy).unwrap();
    std::fs::write(nats.join("jwt/box.jwt"), "resolver-owned claims").unwrap();
    std::fs::create_dir_all(nats.join("js/stream/snapshots")).unwrap();
    std::fs::write(
        nats.join("js/stream/snapshots/data"),
        b"stored messages\x00",
    )
    .unwrap();
    std::fs::write(nats.join("ceremony/root.pub"), raw_hex(&fixture.root)).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&conf_path, std::fs::Permissions::from_mode(0o640)).unwrap();
    }
    let mut before = snapshot(fixture.dir.path());
    let other_mtimes = other_file_mtimes(fixture.dir.path());
    assert_eq!(
        before.remove(Path::new("nats/server.conf")).unwrap(),
        legacy.as_bytes()
    );
    let output = conf_only(&fixture, &[]);
    assert!(output.ok, "{}", output.stderr);
    assert_eq!(output.json["status"], "applied");
    assert_eq!(output.json["health_url"], "http://127.0.0.1:18222/healthz");
    let mut after = snapshot(fixture.dir.path());
    assert_eq!(
        after.remove(Path::new("nats/server.conf")).unwrap(),
        fresh.as_bytes()
    );
    assert_eq!(after, before, "every other file and directory is unchanged");
    assert_eq!(other_file_mtimes(fixture.dir.path()), other_mtimes);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&conf_path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    let before = snapshot(fixture.dir.path());
    let modified = std::fs::metadata(&conf_path).unwrap().modified().unwrap();
    let output = conf_only(&fixture, &[]);
    assert!(output.ok, "{}", output.stderr);
    assert_eq!(output.json["status"], "unchanged");
    assert_eq!(snapshot(fixture.dir.path()), before);
    assert_eq!(
        std::fs::metadata(&conf_path).unwrap().modified().unwrap(),
        modified
    );
}

#[test]
fn conf_only_replaces_only_an_explicit_monitor_port_and_identical_is_untouched() {
    let fixture = Fixture::new();
    fixture.install_with_ports(15222, 19222);
    let conf_path = fixture.nats_dir().join("server.conf");
    let old = std::fs::read_to_string(&conf_path).unwrap();
    let mut before = snapshot(fixture.dir.path());
    let output = conf_only(&fixture, &[]);
    assert!(
        !output.ok,
        "implicit default must not replace an existing port"
    );
    assert!(
        output.stderr.contains("pass --monitor-port explicitly"),
        "{}",
        output.stderr
    );
    assert!(
        output.stderr.contains(conf_path.to_str().unwrap()),
        "{}",
        output.stderr
    );
    assert_eq!(snapshot(fixture.dir.path()), before);

    let modified = std::fs::metadata(&conf_path).unwrap().modified().unwrap();
    let output = conf_only(&fixture, &["--monitor-port", "19222"]);
    assert!(output.ok, "{}", output.stderr);
    assert_eq!(output.json["status"], "unchanged");
    assert_eq!(output.json["health_url"], "http://127.0.0.1:19222/healthz");
    assert_eq!(snapshot(fixture.dir.path()), before);
    assert_eq!(
        std::fs::metadata(&conf_path).unwrap().modified().unwrap(),
        modified
    );

    let output = conf_only(&fixture, &["--monitor-port", "18222"]);
    assert!(output.ok, "{}", output.stderr);
    assert_eq!(output.json["health_url"], "http://127.0.0.1:18222/healthz");
    let mut after = snapshot(fixture.dir.path());
    before.remove(Path::new("nats/server.conf")).unwrap();
    assert_eq!(
        after.remove(Path::new("nats/server.conf")).unwrap(),
        old.replace("http: \"127.0.0.1:19222\"\n", "http: \"127.0.0.1:18222\"\n")
            .as_bytes()
    );
    assert_eq!(after, before);
}

#[test]
fn conf_only_refuses_handwritten_non_loopback_ambiguous_and_equal_ports_without_writes() {
    let fixture = Fixture::new();
    fixture.install_with_ports(15222, 18222);
    let conf_path = fixture.nats_dir().join("server.conf");
    let conf = std::fs::read_to_string(&conf_path).unwrap();
    let no_http = conf.replace("http: \"127.0.0.1:18222\"\n", "");
    let handwritten = conf.lines().skip(1).collect::<Vec<_>>().join("\n");
    let mut refused = vec![handwritten, no_http.replace("15222", "18222")];
    for host in ["0.0.0.0", "192.0.2.1", "::", "::1", "localhost"] {
        refused.push(conf.replace("listen: \"127.0.0.1:", &format!("listen: \"{host}:")));
        refused.push(conf.replace("http: \"127.0.0.1:", &format!("http: \"{host}:")));
    }
    refused.push(format!("{conf}http: \"127.0.0.1:18222\"\n"));
    refused.push(format!("{conf}listen: \"127.0.0.1:15222\"\n"));
    refused.push(conf.replace("listen: \"127.0.0.1:15222\"", "listen: \"127.0.0.1:0\""));
    for invalid in refused {
        std::fs::write(&conf_path, &invalid).unwrap();
        let before = snapshot(fixture.dir.path());
        let output = conf_only(&fixture, &[]);
        assert!(!output.ok, "conf-only must refuse {invalid}");
        assert!(
            output.stderr.contains(conf_path.to_str().unwrap()),
            "{}",
            output.stderr
        );
        assert_eq!(snapshot(fixture.dir.path()), before);
    }
    std::fs::write(&conf_path, conf).unwrap();
    let before = snapshot(fixture.dir.path());
    for extra in [
        ["--monitor-port", "15222"],
        ["--monitor-port", "0"],
        ["--monitor-port", "65536"],
        ["--monitor-port", "127.0.0.1:18222"],
        ["--monitor-host", "0.0.0.0"],
        ["--port", "14222"],
        ["--root-pub", "abcd"],
    ] {
        let output = conf_only(&fixture, &extra);
        assert!(!output.ok, "conf-only must refuse {extra:?}");
        assert_eq!(snapshot(fixture.dir.path()), before);
    }
    // Explicit replacement does not authorize a non-loopback monitoring listener.
    std::fs::write(
        &conf_path,
        std::fs::read_to_string(&conf_path)
            .unwrap()
            .replace("http: \"127.0.0.1:", "http: \"0.0.0.0:"),
    )
    .unwrap();
    let before = snapshot(fixture.dir.path());
    assert!(!conf_only(&fixture, &["--monitor-port", "19222"]).ok);
    assert_eq!(snapshot(fixture.dir.path()), before);
}
