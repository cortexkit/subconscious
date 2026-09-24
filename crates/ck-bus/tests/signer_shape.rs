//! Ladder row "Signer wire shape" (slice 3 of `docs/specs/ck-bus-module.md`). served-by: harness-signer for the golden
//! half; claustrum-binary for the real-binary half, which records
//! `claustrum-binary-absent` and reports SKIP when `CK_CLAUSTRUM_BIN` or `CK_CK_BIN` is
//! missing.
//!
//! The harness signer's `credential.sign` and `credential.public_key` replies are checked
//! byte-for-byte against a committed golden request/reply pair built from the RFC 8032
//! TEST 2 key (`tests/harness/signer/golden/credential_pair.json`, citing claustrum
//! `57a501b`). ck-bus's own request builders must produce the golden requests, and its
//! route and parsers must recover the RFC signature and public key through the daemon.
//! Controls: a signer that pre-hashes, one that signs the base64 text, and one that
//! answers in base64url each diverge from the golden reply and are refused by ck-bus.
//! The production binary's symbol table, dependency tree and source tree are checked
//! for the signer.

#[allow(dead_code)]
#[path = "../src/credentials/mod.rs"]
mod credentials;
#[allow(dead_code)]
#[path = "../src/grants/mod.rs"]
mod grants;
#[allow(dead_code)]
mod harness;
#[allow(dead_code)]
#[path = "../src/runtime/seams.rs"]
mod runtime;

use std::{collections::BTreeSet, path::Path, process::Command};

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use credentials::{
    custody::KeyCustody,
    issue::{sign_user_jwt, IssueError, UserJwtRequest},
    roots::KeyIdLedger,
    vault::{ClaustrumRoute, VaultError, VaultSigning},
    wire::{self, ReplyError, VaultPublicKey, VaultSignature},
};
use harness::{
    report::{Row, RowReport, ServedBy},
    signer::{
        self,
        claustrum::{RealClaustrum, CLAUSTRUM_BINARY_ABSENT},
        run::{ClaustrumSide, SignerRun},
        seeds, HarnessSigner, SignerFault, SIGNER_OPERATIONS,
    },
};
use nkeys::KeyPair;
use serde_json::Value;
use subc_client_rs::{
    consumer::{CallOptions, ConsumerOptions, SubcConsumer},
    ConsumerIdentity, HandlerOutcome,
};
use subc_protocol::{BindIdentity, RouteTarget};

fn signer_vocabulary() -> BTreeSet<String> {
    SIGNER_OPERATIONS
        .iter()
        .map(|op| (*op).to_string())
        .collect()
}

fn golden_signer(golden: &Value) -> HarnessSigner {
    let secret: [u8; 32] = signer::unhex(golden["key"]["secret_key_hex"].as_str().unwrap())
        .try_into()
        .expect("the RFC key is 32 bytes");
    HarnessSigner::fixed(golden["credential_id"].as_str().unwrap(), secret)
}

/// The signer answered in-process, with ck-bus's own wire builders and parsers, so the
/// controls exercise ck-bus's refusal without a daemon.
struct InProcess(HarnessSigner);

fn reply_error(error: ReplyError) -> VaultError {
    match error {
        ReplyError::Refused { code, class } => VaultError::Refused { code, class },
        ReplyError::Malformed(detail) => VaultError::Malformed(detail),
    }
}

fn response_body(outcome: HandlerOutcome) -> Vec<u8> {
    match outcome {
        HandlerOutcome::Response(body) => body,
        other => panic!("the harness signer answered with an error frame: {other:?}"),
    }
}

#[async_trait]
impl VaultSigning for InProcess {
    async fn sign(
        &self,
        credential_id: &str,
        payload: &[u8],
    ) -> Result<VaultSignature, VaultError> {
        let body = wire::sign_request(credential_id, payload).map_err(reply_error)?;
        wire::parse_sign_reply(&response_body(self.0.answer(&body))).map_err(reply_error)
    }

    async fn public_key(&self, credential_id: &str) -> Result<VaultPublicKey, VaultError> {
        let body = wire::public_key_request(credential_id);
        wire::parse_public_key_reply(&response_body(self.0.answer(&body))).map_err(reply_error)
    }
}

/// Names the first field where `actual` departs from the golden reply's shape and
/// encoding: the field set, standard padded base64 of 64 bytes, lowercase-hex key id
/// derived from the public key, and the exact values where the key is the golden key.
fn shape_divergence(golden: &Value, actual: &Value, same_key: bool) -> Option<String> {
    let golden = golden["result"].as_object()?;
    let Some(actual) = actual["result"].as_object() else {
        return Some("reply has no result object".to_string());
    };
    let golden_fields: BTreeSet<_> = golden.keys().collect();
    let actual_fields: BTreeSet<_> = actual.keys().collect();
    if golden_fields != actual_fields {
        return Some(format!(
            "field names {actual_fields:?}, golden {golden_fields:?}"
        ));
    }
    if let Some(signature) = actual.get("signature_b64").and_then(Value::as_str) {
        match STANDARD.decode(signature) {
            Ok(bytes) if bytes.len() == 64 => {}
            Ok(bytes) => return Some(format!("signature_b64 decodes to {} bytes", bytes.len())),
            Err(error) => {
                return Some(format!("signature_b64 not standard padded base64: {error}"))
            }
        }
    }
    if let Some(key_id) = actual.get("key_id").and_then(Value::as_str) {
        if key_id.len() != 16 || wire::decode_hex_lower(key_id).is_none() {
            return Some(format!("key_id {key_id} is not 8 bytes of lowercase hex"));
        }
    }
    if same_key {
        for (field, value) in golden {
            if actual.get(field) != Some(value) {
                return Some(format!(
                    "{field} is {:?}, golden {value:?}",
                    actual.get(field)
                ));
            }
        }
    }
    None
}

#[test]
fn ck_bus_request_builders_emit_the_golden_requests() {
    let golden = signer::golden();
    let credential_id = golden["credential_id"].as_str().unwrap();
    let payload = signer::unhex(golden["key"]["message_hex"].as_str().unwrap());
    let sign: Value =
        serde_json::from_slice(&wire::sign_request(credential_id, &payload).unwrap()).unwrap();
    assert_eq!(sign, golden["sign"]["request"], "credential.sign request");
    let public: Value = serde_json::from_slice(&wire::public_key_request(credential_id)).unwrap();
    assert_eq!(
        public, golden["public_key"]["request"],
        "credential.public_key request"
    );
}

#[test]
fn golden_pair_is_the_rfc_8032_vector() {
    let golden = signer::golden();
    let reply = &golden["sign"]["reply"]["result"];
    let signature = STANDARD
        .decode(reply["signature_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        signer::hex(&signature),
        golden["key"]["signature_hex"].as_str().unwrap(),
        "the golden signature_b64 is the RFC 8032 TEST 2 signature"
    );
    let public: [u8; 32] = signer::unhex(golden["key"]["public_key_hex"].as_str().unwrap())
        .try_into()
        .unwrap();
    assert_eq!(
        wire::key_id_for(&public),
        reply["key_id"].as_str().unwrap(),
        "the golden key_id is sha256(public)[..8]"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signer_shape_harness_half_matches_the_golden_pair() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let golden = signer::golden();
    let signer = golden_signer(&golden);
    let binary = Path::new(env!("CARGO_BIN_EXE_ck-bus"));
    let run = SignerRun::start(SignerRun::tree(), binary, ClaustrumSide::Signer(signer)).await;

    // The raw wire: the golden request bytes through the daemon to the module registered
    // as claustrum, and the reply compared field for field.
    let consumer = SubcConsumer::connect(&run.connection_file, ConsumerOptions::default())
        .await
        .expect("connect to the fixture daemon");
    for op in ["sign", "public_key"] {
        let request = serde_json::to_vec(&golden[op]["request"]).unwrap();
        let reply = consumer
            .call(
                RouteTarget::ManagementSurface {
                    module_id: "claustrum".to_string(),
                },
                BindIdentity::new(run.root.path(), "ck-bus-acceptance", "signer-shape"),
                request,
                CallOptions::default(),
            )
            .await
            .unwrap_or_else(|error| panic!("{op} over the daemon: {error}"));
        let reply: Value = serde_json::from_slice(&reply).expect("reply is JSON");
        assert_eq!(
            reply, golden[op]["reply"],
            "{op} reply must equal the golden reply"
        );
    }
    consumer.close().await;

    // ck-bus's own route and parsers recover the RFC signature and public key.
    let credential_id = golden["credential_id"].as_str().unwrap();
    let route = ClaustrumRoute::new(run.connection_file.clone(), None);
    let payload = signer::unhex(golden["key"]["message_hex"].as_str().unwrap());
    let signature = route.sign(credential_id, &payload).await.expect("sign");
    assert_eq!(
        signer::hex(&signature.signature),
        golden["key"]["signature_hex"].as_str().unwrap()
    );
    let public = route.public_key(credential_id).await.expect("public key");
    assert_eq!(
        signer::hex(&public.public),
        golden["key"]["public_key_hex"].as_str().unwrap()
    );
    assert_eq!(signature.key_id, public.key_id);

    run.shutdown().await;
    RowReport::passed(Row::SignerShape)
        .served_by(ServedBy::HarnessSigner)
        .reached("credential.sign")
        .reached("credential.public_key")
        .emit(&signer_vocabulary());
}

#[tokio::test]
async fn control_faulty_signers_diverge_from_the_golden_and_are_refused_by_ck_bus() {
    let golden = signer::golden();
    let sign_request = serde_json::to_vec(&golden["sign"]["request"]).unwrap();
    let account = grants::derive_account("box_goldenfixture").unwrap();
    let custody = KeyCustody::new();
    let user = custody.generate_user();
    let grant = grants::participant_grant(&account, &user, &["agent_gold_a"], &[]).unwrap();

    for fault in [
        SignerFault::Faithful,
        SignerFault::PreHashes,
        SignerFault::SignsBase64Text,
        SignerFault::ReturnsBase64Url,
    ] {
        let signer = golden_signer(&golden).with_fault(fault);
        let reply: Value = serde_json::from_slice(&response_body(signer.answer(&sign_request)))
            .expect("reply is JSON");
        let divergence = shape_divergence(&golden["sign"]["reply"], &reply, true);
        let issued = sign_user_jwt(
            &InProcess(signer),
            &KeyIdLedger::new(),
            &UserJwtRequest {
                root_credential_id: golden["credential_id"].as_str().unwrap(),
                user_public: &user,
                issuer_account: None,
                name: "signer-shape-control",
                issued_at: 1_790_000_000,
                grant: &grant,
            },
        )
        .await;
        match fault {
            SignerFault::Faithful => {
                assert_eq!(
                    divergence, None,
                    "the faithful signer matches the golden reply"
                );
                issued.expect("ck-bus issues under the faithful signer");
            }
            SignerFault::PreHashes | SignerFault::SignsBase64Text => {
                let divergence = divergence.expect("a wrong signature must diverge");
                assert!(
                    divergence.starts_with("signature_b64"),
                    "{fault:?}: {divergence}"
                );
                assert!(
                    matches!(issued, Err(IssueError::SignatureDoesNotVerify { .. })),
                    "{fault:?}: ck-bus must refuse a signature over the wrong bytes: {issued:?}"
                );
            }
            SignerFault::ReturnsBase64Url => {
                let divergence = divergence.expect("base64url must diverge");
                assert!(
                    divergence.contains("not standard padded base64"),
                    "{fault:?}: {divergence}"
                );
                assert!(
                    matches!(issued, Err(IssueError::Vault(VaultError::Malformed(ref d))) if d.contains("signature_b64")),
                    "{fault:?}: ck-bus must refuse a base64url signature: {issued:?}"
                );
            }
        }
    }
}

#[test]
fn fixtures_carry_no_production_root() {
    let text = std::fs::read_to_string(signer::golden_path()).unwrap();
    for production in [
        signer::PRODUCTION_CREDENTIAL_ID,
        signer::PRODUCTION_PUBLIC_KEY_HEX,
        signer::PRODUCTION_KEY_ID,
    ] {
        assert!(
            !text.contains(production),
            "the golden pair must not carry production material {production}"
        );
    }
    // The guard is on key material: a supervised ck-bus asks for its roots by their
    // production credential ids, so the harness signer answers under those ids, but
    // only ever with throwaway keys.
    for (public_key_hex, key_id) in [
        (signer::PRODUCTION_PUBLIC_KEY_HEX, "00"),
        ("00", signer::PRODUCTION_KEY_ID),
    ] {
        let refused = std::panic::catch_unwind(|| {
            signer::assert_not_production(signer::PRODUCTION_CREDENTIAL_ID, public_key_hex, key_id)
        });
        assert!(
            refused.is_err(),
            "the guard must fail a fixture holding the production root's key material"
        );
    }
    signer::assert_not_production(signer::PRODUCTION_CREDENTIAL_ID, "00", "00");
}

/// The production binary carries no signer. Three reads, each with a positive control
/// run against something known to carry it, so an empty result means absence and not a
/// scan that cannot see.
#[test]
fn production_binary_has_no_path_to_the_harness_signer() {
    let production = Path::new(env!("CARGO_BIN_EXE_ck-bus"));
    let this_test = std::env::current_exe().expect("current test binary");
    // The symbol read runs only where `nm` can see a Rust binary's symbols. A
    // Windows MSVC executable keeps them in a separate .pdb, so `nm` returns
    // nothing and the positive control below fails by design. There the
    // dependency-tree and source reads still prove the signer is absent.
    #[cfg(not(windows))]
    {
        let symbols = |binary: &Path| {
            let output = Command::new("nm")
                .arg(binary)
                .output()
                .unwrap_or_else(|error| panic!("nm {}: {error}", binary.display()));
            assert!(output.status.success(), "nm {} failed", binary.display());
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        assert!(
            symbols(&this_test).contains("HarnessSigner"),
            "control: this test binary links the signer, so the symbol scan must see it"
        );
        assert!(
            !symbols(production).contains("HarnessSigner"),
            "the production ck-bus binary must not contain the harness signer"
        );
    }
    #[cfg(windows)]
    let _ = (production, this_test);

    let tree = Command::new(env!("CARGO"))
        .args([
            "tree",
            "-p",
            "ck-bus",
            "--locked",
            "--offline",
            "--prefix",
            "none",
        ])
        .args(["-e", "normal,build"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo tree");
    let tree = String::from_utf8_lossy(&tree.stdout);
    assert!(
        tree.lines().any(|line| line.starts_with("ck-bus ")),
        "control: the dependency tree read must list ck-bus itself: {tree}"
    );
    for line in tree.lines() {
        let lower = line.to_lowercase();
        assert!(
            !lower.contains("harness") && !lower.contains("signer"),
            "production dependency tree carries a signer crate: {line}"
        );
    }

    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mentions = |dir: &Path| -> Vec<String> {
        let mut found = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(path) = pending.pop() {
            if path.is_dir() {
                pending.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                if text.contains("mod harness") || text.contains("HarnessSigner") {
                    found.push(path.display().to_string());
                }
            }
        }
        found
    };
    assert!(
        !mentions(&crate_dir.join("tests")).is_empty(),
        "control: the source scan must find the harness where it lives"
    );
    assert_eq!(
        mentions(&crate_dir.join("src")),
        Vec::<String>::new(),
        "no production source may reach the harness"
    );
}

/// The same requests against a real claustrum. The ceremony cannot import a fixed key
/// (`mint-signing-key` generates inside the vault), so the check verifies the signature
/// under the returned public key and recomputes `key_id` instead of comparing bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signer_shape_real_binary_half() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let tree = SignerRun::tree();
    let real = match RealClaustrum::discover(&SignerRun::data_home(&tree), &tree.join("keys")) {
        Ok(real) => real,
        Err(observation) => {
            RowReport::skipped(Row::SignerShape, CLAUSTRUM_BINARY_ABSENT, observation)
                .served_by(ServedBy::ClaustrumBinary)
                .emit(&signer_vocabulary());
            return;
        }
    };
    std::fs::create_dir_all(tree.join("keys")).unwrap();
    let golden = signer::golden();
    let credential_id = "signing:harness-shape:1";
    let (printed_public, printed_key_id) = real.bootstrap_and_mint(credential_id);
    real.grant(credential_id, "sign");
    real.grant(credential_id, "read");
    signer::assert_not_production(credential_id, &printed_public, &printed_key_id);

    let binary = Path::new(env!("CARGO_BIN_EXE_ck-bus"));
    let run = SignerRun::start(tree, binary, ClaustrumSide::Binary(&real)).await;
    let identity = ckbus_identity(&run).await;
    let consumer = SubcConsumer::connect(&run.connection_file, ConsumerOptions::default())
        .await
        .expect("connect to the fixture daemon");
    let mut replies = Vec::new();
    for op in ["sign", "public_key"] {
        let mut request = golden[op]["request"].clone();
        request["params"]["credential_id"] = Value::String(credential_id.to_string());
        let reply = consumer
            .call(
                RouteTarget::ManagementSurface {
                    module_id: "claustrum".to_string(),
                },
                BindIdentity::new(run.root.path(), "ck-bus-acceptance", "signer-shape-real"),
                serde_json::to_vec(&request).unwrap(),
                CallOptions {
                    consumer_identity: Some(identity.clone()),
                    ..CallOptions::default()
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{op} against the real claustrum: {error}"));
        let reply: Value = serde_json::from_slice(&reply).expect("reply is JSON");
        if let Some(divergence) = shape_divergence(&golden[op]["reply"], &reply, false) {
            panic!("real claustrum {op} diverges from the golden shape: {divergence}: {reply}");
        }
        replies.push(reply);
    }
    consumer.close().await;
    let public_hex = replies[1]["result"]["public_key_hex"].as_str().unwrap();
    assert_eq!(
        public_hex, printed_public,
        "public_key_hex matches the ceremony output"
    );
    let public: [u8; 32] = wire::decode_hex_lower(public_hex)
        .unwrap()
        .try_into()
        .unwrap();
    assert_eq!(
        wire::key_id_for(&public),
        printed_key_id,
        "key_id is sha256(public)[..8]"
    );
    assert_eq!(
        replies[0]["result"]["key_id"],
        replies[1]["result"]["key_id"]
    );
    let signature = STANDARD
        .decode(replies[0]["result"]["signature_b64"].as_str().unwrap())
        .unwrap();
    let issuer = credentials::nkey::encode_public(credentials::nkey::NkeyRole::Account, &public);
    KeyPair::from_public_key(&issuer)
        .unwrap()
        .verify(
            &signer::unhex(golden["key"]["message_hex"].as_str().unwrap()),
            &signature,
        )
        .expect("the real signature is pure Ed25519 over the decoded payload");

    let root = run.shutdown().await;
    assert!(seeds::scan_tree(&root.join("run/logs")).is_empty());
    eprintln!(
        "claustrum-binary: {} ({}), ck: {} ({})",
        real.claustrum_bin.display(),
        real.claustrum_version,
        real.ck_bin.display(),
        real.ck_version
    );
    RowReport::passed(Row::SignerShape)
        .served_by(ServedBy::ClaustrumBinary)
        .reached("credential.sign")
        .reached("credential.public_key")
        .emit(&signer_vocabulary());
}

/// ck-bus's consumer identity, read from the supervised process the daemon launched
/// (its environment carries the launch nonce the daemon injected). The daemon, not the
/// test, stamps the resulting principal.
async fn ckbus_identity(run: &SignerRun) -> ConsumerIdentity {
    let pid = run.supervised_pid("ckbus").await;
    let environment = seeds::process_environment(pid);
    ConsumerIdentity {
        module_id: "ckbus".to_string(),
        launch_nonce: seeds::environment_value(&environment, "SUBC_LAUNCH_NONCE")
            .expect("the supervised ckbus carries SUBC_LAUNCH_NONCE"),
    }
}
