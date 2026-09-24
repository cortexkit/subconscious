//! The harness signer: a module registered as `claustrum` that answers
//! `credential.sign` and `credential.public_key` with real signatures and real public
//! halves, in exactly claustrum's wire shape, from throwaway fixture keys.
//!
//! It proves what ck-bus builds and how a real `nats-server` judges it. It proves no
//! vault authority: it answers every principal the same way, and a row that asserts
//! authorization against it is refused by the report.
//!
//! This directory exists only under `tests/`; the production binary has no path to it.
//! Every row file compiles the whole harness, so items a given row does not use are
//! expected here.
#![allow(dead_code)]

pub mod claustrum;
pub mod nats;
pub mod run;
pub mod seeds;

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use nkeys::{KeyPair, KeyPairType};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subc_client_rs::{BindDecision, HandlerOutcome, ModuleHandler, RequestCtx, RouteBindRequest};
use subc_protocol::{
    manifest::{
        Concurrency, ManagementOperation, ManagementOperationKind, ModuleManifest, ProviderRole,
    },
    Principal,
};

/// The signer's whole vocabulary.
pub const SIGNER_OPERATIONS: &[&str] = &["credential.sign", "credential.public_key"];

/// The operator's real box-account root. A fixture holding its public key or `key_id`
/// is using production material, and the harness fails the run. The credential id alone
/// is not material: a supervised ck-bus asks for its roots by their production ids, and
/// the signer answers under them with throwaway keys.
pub const PRODUCTION_CREDENTIAL_ID: &str = "signing:ck-bus-account:1";
pub const PRODUCTION_PUBLIC_KEY_HEX: &str =
    "c73fe2b0df0d9921f4531bf1277404839a6d630be49ed77dbd29848f3e1bfcfa";
pub const PRODUCTION_KEY_ID: &str = "0253fac9609168a5";

/// The vault's signing-payload cap (`signing.rs::MAX_SIGN_PAYLOAD`).
const MAX_SIGN_PAYLOAD: usize = 1024 * 1024;

/// A deliberately wrong signer, for the controls that prove the shape check can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignerFault {
    /// Claustrum's behaviour: pure Ed25519 over the decoded bytes, standard padded base64.
    Faithful,
    /// Signs a SHA-256 digest of the bytes instead of the bytes.
    PreHashes,
    /// Signs the base64 text of the payload instead of the decoded bytes.
    SignsBase64Text,
    /// Returns the signature as unpadded base64url.
    ReturnsBase64Url,
}

/// A fixture root: the key pair and the public half as the vault reports it.
#[derive(Clone)]
pub struct FixtureRoot {
    pub pair: KeyPair,
    pub public: [u8; 32],
}

impl FixtureRoot {
    fn from_pair(pair: KeyPair) -> Self {
        let (_, public) = nkeys::from_public_key(&pair.public_key())
            .expect("a key pair's own public key always decodes");
        Self { pair, public }
    }

    pub fn public_key_hex(&self) -> String {
        hex(&self.public)
    }

    pub fn key_id(&self) -> String {
        hex(&Sha256::digest(self.public)[..8])
    }

    /// The root as an account nkey (`A...`), the form a JWT names its issuer in.
    pub fn account_public(&self) -> String {
        self.pair.public_key()
    }
}

#[derive(Clone)]
pub struct HarnessSigner {
    roots: Arc<BTreeMap<String, FixtureRoot>>,
    fault: SignerFault,
    principals: Arc<Mutex<Vec<Option<Principal>>>>,
}

impl HarnessSigner {
    /// A signer holding fresh random roots under the given fixture credential ids.
    pub fn generated(credential_ids: &[&str]) -> Self {
        Self::from_roots(credential_ids.iter().map(|id| {
            (
                (*id).to_string(),
                FixtureRoot::from_pair(KeyPair::new(KeyPairType::Account)),
            )
        }))
    }

    /// A signer holding one root with a fixed secret (the golden pair's RFC 8032 key).
    pub fn fixed(credential_id: &str, secret: [u8; 32]) -> Self {
        let pair = KeyPair::new_from_raw(KeyPairType::Account, secret)
            .expect("any 32 bytes are an Ed25519 seed");
        Self::from_roots([(credential_id.to_string(), FixtureRoot::from_pair(pair))])
    }

    /// A signer holding the given key pairs as roots. The pair's type decides only how
    /// `FixtureRoot::account_public` spells the key; the signature is the same.
    pub fn from_pairs(roots: impl IntoIterator<Item = (String, KeyPair)>) -> Self {
        Self::from_roots(
            roots
                .into_iter()
                .map(|(id, pair)| (id, FixtureRoot::from_pair(pair))),
        )
    }

    fn from_roots(roots: impl IntoIterator<Item = (String, FixtureRoot)>) -> Self {
        let roots: BTreeMap<_, _> = roots.into_iter().collect();
        for (credential_id, root) in &roots {
            assert_not_production(credential_id, &root.public_key_hex(), &root.key_id());
        }
        Self {
            roots: Arc::new(roots),
            fault: SignerFault::Faithful,
            principals: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The same signer without the root `credential_id`, which it then answers
    /// `not_found` for, like a vault missing the key or ck-bus's grant on it.
    pub fn without(&self, credential_id: &str) -> Self {
        let mut roots = self.roots.as_ref().clone();
        roots.remove(credential_id);
        Self {
            roots: Arc::new(roots),
            fault: self.fault,
            principals: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn with_fault(mut self, fault: SignerFault) -> Self {
        self.fault = fault;
        self
    }

    pub fn root(&self, credential_id: &str) -> &FixtureRoot {
        self.roots
            .get(credential_id)
            .unwrap_or_else(|| panic!("the harness signer holds no root {credential_id}"))
    }

    pub fn observed_principals(&self) -> Vec<Option<Principal>> {
        self.principals
            .lock()
            .expect("signer principal log lock must remain usable")
            .clone()
    }

    /// Registered the way claustrum registers its read surface: a management surface
    /// (`credentials-module/src/main.rs::manifest` at 57a501b), so ck-bus reaches it by
    /// the same route target it uses in production.
    pub fn manifest(&self) -> ModuleManifest {
        let operations = SIGNER_OPERATIONS
            .iter()
            .map(|name| ManagementOperation {
                name: (*name).to_string(),
                kind: ManagementOperationKind::Query,
                description: Some("acceptance harness signer".to_string()),
            })
            .collect();
        ModuleManifest::builder("claustrum", "0.0.0-harness-signer")
            .provides(vec![ProviderRole::ManagementSurface {
                operations,
                config_schema: json!({}),
                observability: vec![],
                identity_scope: vec![],
                concurrency: Concurrency::ModuleManaged,
            }])
            .build()
    }

    /// Answers one request body the way claustrum's read surface does: a `{method,
    /// params}` body in, `{"result": ...}` out, a vault refusal as `{"result": {"error":
    /// {code, class}}}` in an ordinary response, and undecodable params as an error
    /// frame (`invalid_params`).
    pub fn answer(&self, body: &[u8]) -> HandlerOutcome {
        let Ok(request) = serde_json::from_slice::<Value>(body) else {
            return invalid_params("request body is not JSON");
        };
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        let credential_id = params.get("credential_id").and_then(Value::as_str);
        match request.get("method").and_then(Value::as_str) {
            Some("credential.sign") => {
                let (Some(credential_id), Some(payload_b64)) = (
                    credential_id,
                    params.get("payload_b64").and_then(Value::as_str),
                ) else {
                    return invalid_params(
                        "credential.sign requires credential_id and payload_b64",
                    );
                };
                let Ok(payload) = STANDARD.decode(payload_b64) else {
                    return invalid_params("payload_b64 is not standard base64");
                };
                let Some(root) = self.roots.get(credential_id) else {
                    return refusal("not_found", "permanent");
                };
                if payload.len() > MAX_SIGN_PAYLOAD {
                    return refusal("sign_payload_too_large", "context_overflow");
                }
                let signed: Vec<u8> = match self.fault {
                    SignerFault::PreHashes => Sha256::digest(&payload).to_vec(),
                    SignerFault::SignsBase64Text => payload_b64.as_bytes().to_vec(),
                    SignerFault::Faithful | SignerFault::ReturnsBase64Url => payload,
                };
                let signature = root.pair.sign(&signed).expect("fixture roots hold seeds");
                let signature_b64 = match self.fault {
                    SignerFault::ReturnsBase64Url => URL_SAFE_NO_PAD.encode(&signature),
                    _ => STANDARD.encode(&signature),
                };
                result(json!({"signature_b64": signature_b64, "key_id": root.key_id()}))
            }
            Some("credential.public_key") => {
                let Some(credential_id) = credential_id else {
                    return invalid_params("credential.public_key requires credential_id");
                };
                let Some(root) = self.roots.get(credential_id) else {
                    return refusal("not_found", "permanent");
                };
                result(json!({
                    "public_key_hex": root.public_key_hex(),
                    "key_id": root.key_id(),
                    "algorithm": "ed25519",
                }))
            }
            Some(other) => HandlerOutcome::Error {
                code: "unknown_operation".to_string(),
                message: format!("the harness signer does not serve {other}"),
            },
            None => invalid_params("request has no method"),
        }
    }
}

fn result(value: Value) -> HandlerOutcome {
    HandlerOutcome::Response(
        serde_json::to_vec(&json!({ "result": value })).expect("reply encodes"),
    )
}

fn refusal(code: &str, class: &str) -> HandlerOutcome {
    result(json!({"error": {"code": code, "class": class}}))
}

fn invalid_params(detail: &str) -> HandlerOutcome {
    HandlerOutcome::Error {
        code: "invalid_params".to_string(),
        message: format!("params not decodable: {detail}"),
    }
}

#[async_trait]
impl ModuleHandler for HarnessSigner {
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        self.answer(&body)
    }

    /// Accepts every caller. The principal is recorded for the report, never checked:
    /// this signer carries no authority to check it against.
    async fn on_bind(&self, request: &RouteBindRequest) -> BindDecision {
        self.principals
            .lock()
            .expect("signer principal log lock must remain usable")
            .push(request.principal.clone());
        BindDecision::accept()
    }
}

/// Fails the run when fixture material is the operator's production root.
pub fn assert_not_production(credential_id: &str, public_key_hex: &str, key_id: &str) {
    let _ = credential_id;
    assert_ne!(
        public_key_hex, PRODUCTION_PUBLIC_KEY_HEX,
        "a harness fixture must never hold the production root's public key"
    );
    assert_ne!(
        key_id, PRODUCTION_KEY_ID,
        "a harness fixture must never carry the production root's key_id"
    );
}

pub fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/harness/signer/golden/credential_pair.json")
}

pub fn golden() -> Value {
    let path = golden_path();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&text).expect("the golden pair is JSON")
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("fixture hex"))
        .collect()
}
