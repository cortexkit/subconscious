use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use nkeys::KeyPair;
use rand_core::{OsRng, TryRngCore as _};
use serde_json::{json, Value};
use subc_core::setup::bus::sign_ceremony_input;
use tokio::{process::Child, time::Instant};

struct Install {
    dir: tempfile::TempDir,
    nats: PathBuf,
    signer: KeyPair,
    sysaccount: KeyPair,
}

impl Install {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let nats = dir.path().join("nats");
        Self {
            dir,
            nats,
            signer: KeyPair::new_operator(),
            sysaccount: KeyPair::new_account(),
        }
    }

    fn ck_bus(&self, command: &str, root: &VerifyingKey, extra: &[&str]) -> Value {
        let binary = cortexkit_test_support::ckdev_binary(env!("CARGO_BIN_EXE_ck-bus"));
        let output = cortexkit_test_support::dev_command(binary)
            .args([
                command,
                "--nats-dir",
                self.nats.to_str().unwrap(),
                "--root-pub",
                &hex(root.as_bytes()),
            ])
            .args(extra)
            .env_remove("SUBC_MODULE_ID")
            .env("XDG_DATA_HOME", self.dir.path().join("data"))
            .env("XDG_RUNTIME_DIR", self.dir.path().join("run"))
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .stdin(Stdio::null())
            .output()
            .expect("run ck-bus");
        assert!(
            output.status.success(),
            "ck-bus {command} exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("ck-bus returned JSON")
    }

    fn plan(&self, root: &VerifyingKey) -> Value {
        let public_hex = |key: &KeyPair| hex(&nkeys::from_public_key(&key.public_key()).unwrap().1);
        self.ck_bus(
            "install-plan",
            root,
            &[
                "--signer-pub",
                &public_hex(&self.signer),
                "--sysaccount-pub",
                &public_hex(&self.sysaccount),
            ],
        )
    }

    fn sign_and_apply(&self, root: SigningKey) -> (String, Value) {
        let public = root.verifying_key();
        let plan = self.plan(&public);
        assert_eq!(plan["status"], "ceremony needed", "{plan}");
        let system_account = plan["system_account"].as_str().unwrap().to_owned();
        let payloads = plan["payloads"].as_array().unwrap();
        assert_eq!(payloads.len(), 2, "{plan}");
        let sign = |name: &str| {
            let payload = payloads.iter().find(|p| p["payload"] == name).unwrap();
            let path = payload["path"].as_str().unwrap();
            assert_eq!(
                Path::new(path),
                self.nats.join("ceremony").join(format!("{name}.input"))
            );
            let bytes = fs::read(path).unwrap();
            (path, sign_ceremony_input(&root, &bytes))
        };
        let (operator, operator_sig) = sign("operator");
        let (account, account_sig) = sign("system_account");
        drop(root);
        let applied = self.ck_bus(
            "install-apply",
            &public,
            &[
                "--operator-input",
                operator,
                "--operator-sig",
                &operator_sig,
                "--sysaccount-input",
                account,
                "--sysaccount-sig",
                &account_sig,
            ],
        );
        assert_eq!(applied["status"], "applied", "{applied}");
        assert_eq!(applied["env"]["CKBUS_SYSTEM_ACCOUNT"], system_account);
        (system_account, applied)
    }

    fn verify_operator(&self, root: &VerifyingKey, system_account: &str, jwt: &str) {
        let claims = verify_root_jwt(root, jwt);
        assert_eq!(claims["sub"], claims["iss"]);
        assert_eq!(claims["nats"]["type"], "operator");
        assert_eq!(claims["nats"]["system_account"], system_account);
        assert_eq!(
            claims["nats"]["signing_keys"],
            json!([self.signer.public_key()])
        );
    }

    fn verify_account(&self, root: &VerifyingKey, system_account: &str, jwt: &str) -> Value {
        let claims = verify_root_jwt(root, jwt);
        assert_eq!(claims["sub"], system_account);
        assert_eq!(claims["nats"]["type"], "account");
        assert_eq!(
            claims["nats"]["signing_keys"],
            json!([self.sysaccount.public_key()])
        );
        claims
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn new_root() -> SigningKey {
    let mut seed = [0; 32];
    OsRng.try_fill_bytes(&mut seed).unwrap();
    SigningKey::from_bytes(&seed)
}

fn verify_root_jwt(root: &VerifyingKey, jwt: &str) -> Value {
    let (input, signature) = jwt.trim().rsplit_once('.').unwrap();
    let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(signature).unwrap()).unwrap();
    let (_, claims) = input.split_once('.').unwrap();
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).unwrap()).unwrap();
    root.verify_strict(input.as_bytes(), &signature)
        .unwrap_or_else(|error| {
            panic!(
                "JWT issuer {} does not verify under root {}: {error}",
                claims["iss"],
                hex(root.as_bytes())
            );
        });
    let issuer = claims["iss"].as_str().unwrap();
    let (_, issuer_bytes) = nkeys::from_public_key(issuer).expect("issuer is an nkey");
    assert!(
        issuer.starts_with('O'),
        "issuer is not an operator: {issuer}"
    );
    assert_eq!(issuer_bytes.as_slice(), root.as_bytes());
    claims
}

fn preloaded_account(conf: &str, account: &str) -> String {
    let (_, preload) = conf
        .split_once("resolver_preload {")
        .expect("resolver preload");
    preload
        .split_once('}')
        .unwrap()
        .0
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find_map(|(key, jwt)| {
            (key.trim() == account).then(|| serde_json::from_str(jwt.trim()).unwrap())
        })
        .expect("system account JWT in resolver preload")
}

#[test]
fn setup_signing_installs_and_keeps_signed_jwts() {
    let install = Install::new();
    let root = new_root();
    let public = root.verifying_key();
    let (account, applied) = install.sign_and_apply(root);
    let conf_path = install.nats.join("server.conf");
    let operator_path = install.nats.join("operator.jwt");
    assert_eq!(applied["server_conf"], conf_path.to_str().unwrap());
    assert_eq!(
        applied["env"]["CKBUS_OPERATOR_JWT"],
        operator_path.to_str().unwrap()
    );
    assert_eq!(applied["env"]["CKBUS_NATS_URL"], "nats://127.0.0.1:14222");
    let conf = fs::read_to_string(&conf_path).unwrap();
    let operator = fs::read_to_string(&operator_path).unwrap();
    install.verify_operator(&public, &account, &operator);
    install.verify_account(&public, &account, &preloaded_account(&conf, &account));

    let second_plan = install.plan(&public);
    assert_eq!(second_plan["status"], "no ceremony needed", "{second_plan}");
    assert_eq!(second_plan["system_account"], account);

    let reapplied = install.ck_bus("install-apply", &public, &[]);
    assert_eq!(reapplied["status"], "applied");
    assert_eq!(reapplied["env"], applied["env"]);
    assert_eq!(fs::read(conf_path).unwrap(), conf.as_bytes());
    assert_eq!(fs::read(operator_path).unwrap(), operator.as_bytes());
}

struct Broker {
    child: Child,
    logs: tempfile::TempDir,
    url: String,
}

impl Broker {
    async fn start(bin: &Path, install: &Install) -> Self {
        let logs = tempfile::tempdir_in(install.dir.path()).unwrap();
        let stderr = logs.path().join("stderr");
        // Let the kernel pick the ports without changing the config, resolver, or data dirs.
        let mut child = tokio::process::Command::new(bin)
            .arg("-c")
            .arg(install.nats.join("server.conf"))
            .args(["-p", "-1", "-m", "-1", "--ports_file_dir"])
            .arg(logs.path())
            .stdout(Stdio::null())
            .stderr(fs::File::create(&stderr).unwrap())
            .kill_on_drop(true)
            .spawn()
            .expect("start nats-server with the installed config");
        let deadline = Instant::now() + Duration::from_secs(20);
        let url = loop {
            let url = fs::read_dir(logs.path())
                .unwrap()
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "ports"))
                .find_map(|path| {
                    let ports: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
                    Some(ports["nats"][0].as_str()?.to_owned())
                });
            if let Some(url) = url {
                break url;
            }
            let status = child.try_wait().unwrap();
            assert!(
                status.is_none() && Instant::now() < deadline,
                "nats-server did not publish a ports file (exit: {status:?}):\n{}",
                fs::read_to_string(&stderr).unwrap_or_default()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        Self { child, logs, url }
    }

    async fn connect_system_user(&self, account: &str, signer: &KeyPair) -> async_nats::Client {
        let user = Arc::new(KeyPair::new_user());
        let claims = json!({
            "jti": format!("TEST{}", user.public_key()),
            "iat": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() - 60,
            "iss": signer.public_key(),
            "name": "setup-signing-system-user",
            "sub": user.public_key(),
            "nats": {
                "type": "user", "version": 2, "issuer_account": account,
                "pub": {}, "sub": {}, "subs": -1, "data": -1, "payload": -1,
            },
        });
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"ed25519-nkey","typ":"JWT"}"#),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let signature = URL_SAFE_NO_PAD.encode(signer.sign(input.as_bytes()).unwrap());
        async_nats::ConnectOptions::with_jwt(format!("{input}.{signature}"), move |nonce| {
            let user = user.clone();
            async move { user.sign(&nonce).map_err(async_nats::AuthError::new) }
        })
        .connection_timeout(Duration::from_secs(2))
        .request_timeout(Some(Duration::from_secs(5)))
        .connect(&self.url)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "the system user for {account} failed to connect: {error}\n{}",
                fs::read_to_string(self.logs.path().join("stderr")).unwrap_or_default()
            )
        })
    }

    async fn stop(mut self) {
        self.child.kill().await.expect("stop and reap nats-server");
    }
}

async fn lookup_claim(client: &async_nats::Client, account: &str) -> String {
    let reply = client
        .request(
            format!("$SYS.REQ.ACCOUNT.{account}.CLAIMS.LOOKUP"),
            Vec::new().into(),
        )
        .await
        .expect("system account claims lookup answers");
    String::from_utf8(reply.payload.to_vec()).unwrap()
}

#[tokio::test]
async fn setup_signing_re_roots_populated_resolver() {
    let (bin, version) = match cortexkit_bus_testkit::nats_server_bin() {
        Ok(found) => found,
        Err(error) => {
            cortexkit_bus_testkit::skip_or_fail(error);
            return;
        }
    };
    let install = Install::new();
    let old_root = new_root();
    let old_public = old_root.verifying_key();
    let (account, applied) = install.sign_and_apply(old_root);
    let persisted_path = install.nats.join("jwt").join(format!("{account}.jwt"));

    let broker = Broker::start(&bin, &install).await;
    let client = broker
        .connect_system_user(&account, &install.sysaccount)
        .await;
    let old_jwt = lookup_claim(&client, &account).await;
    let old_claims = install.verify_account(&old_public, &account, &old_jwt);
    assert_eq!(
        fs::read_to_string(&persisted_path).unwrap().trim(),
        old_jwt.trim()
    );
    eprintln!(
        "{version}: populated resolver served system account {account} under the original root (iat {})",
        old_claims["iat"]
    );
    drop(client);
    broker.stop().await;

    let new_root = new_root();
    let new_public = new_root.verifying_key();
    assert_ne!(old_public, new_public);
    let (re_rooted_account, re_rooted) = install.sign_and_apply(new_root);
    assert_eq!(re_rooted_account, account);
    assert_eq!(re_rooted["env"], applied["env"]);
    let operator = fs::read_to_string(install.nats.join("operator.jwt")).unwrap();
    install.verify_operator(&new_public, &account, &operator);
    // Restart with the old claim still in the resolver. Checking only the new preload
    // in server.conf would not show which JWT the restarted broker actually serves.
    assert_eq!(
        fs::read_to_string(&persisted_path).unwrap().trim(),
        old_jwt.trim()
    );
    let broker = Broker::start(&bin, &install).await;
    let client = broker
        .connect_system_user(&account, &install.sysaccount)
        .await;
    let served_jwt = lookup_claim(&client, &account).await;
    assert_ne!(
        served_jwt.trim(),
        old_jwt.trim(),
        "broker kept serving the old JWT for {account}"
    );
    let new_claims = install.verify_account(&new_public, &account, &served_jwt);
    eprintln!(
        "{version}: restarted on the same data directory; a fresh system user signed by the unchanged signer connected and fetched a claim verified under the new root (iat {})",
        new_claims["iat"]
    );
    drop(client);
    broker.stop().await;
}
