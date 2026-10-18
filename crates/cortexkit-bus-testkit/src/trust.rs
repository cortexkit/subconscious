//! Throwaway operator, system-account and account signing keys.

use crate::signer::HarnessSigner;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use nkeys::KeyPair;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs() as i64
}

/// The credential id the operator minted `signing:<provider>:1` under.
pub fn root_id(provider: &str) -> String {
    format!("signing:{provider}:1")
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

pub(crate) fn encode_jwt(
    signer: &KeyPair,
    subject: &str,
    name: &str,
    iat: i64,
    nats: Value,
) -> String {
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
#[derive(Clone)]
pub struct TrustChain {
    pub signer: HarnessSigner,
    pub(crate) operator_root: KeyPair,
    /// The system account's public nkey. Its private identity key is discarded;
    /// user JWTs are signed with a separate throwaway signing key.
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

    /// The public nkey (`A...`) trusted to sign users of the box account.
    /// The box account JWT lists this key in `signing_keys`.
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
