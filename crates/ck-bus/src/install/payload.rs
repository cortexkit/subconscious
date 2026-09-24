//! The two root-signed JWTs of the install ceremony (`docs/designs/nats-install-trust-chain.md`,
//! sections 1 and 3): their claims, their signing inputs, and the checks that bind a
//! signing input to the claims shown for approval.
//!
//! Server rules the claims rely on (nats-server v2.15.0, cited in the design as N1-N8):
//! - The operator JWT is self-signed by the root (`iss` = `sub` = root `O...`) and lists
//!   the operator signer in `signing_keys`. `strict_signing_key_usage` is false because
//!   the root, the operator identity, signs the system account JWT; a strict operator
//!   would drop its identity key from the trusted keys (N1).
//! - `system_account` is set in the operator JWT and in `server.conf`, with the same value,
//!   which the directory resolver requires (N3).
//! - The system account JWT lists the system account signing key in `signing_keys`, so
//!   ck-bus's system user, signed by that key with `issuer_account` set, is accepted (N2).
//! - Every account limit is explicit: a missing limit decodes as 0, which the server
//!   enforces as "none allowed" (N8). JetStream stays off in the system account, so its
//!   storage limits are an explicit 0.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use data_encoding::BASE32_NOPAD;
use nkeys::KeyPair;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::credentials::jwt::JWT_HEADER;

pub const OPERATOR_NAME: &str = "ckbus-operator";
pub const SYSTEM_ACCOUNT_NAME: &str = "SYS";

/// Which of the two ceremony payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Payload {
    Operator,
    SystemAccount,
}

impl Payload {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::SystemAccount => "system_account",
        }
    }
}

/// The public keys pinned beside the root, as nkeys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedKeys {
    /// The operator identity (`O...`): signs both payloads.
    pub root: String,
    /// The operator signing key (`O...`): signs box account JWTs at runtime.
    pub signer: String,
    /// The system account signing key (`A...`): signs ck-bus's system users.
    pub sysaccount: String,
}

/// Adds the `jti`: a digest of the claims written with an empty `jti`, the same
/// derivation ck-bus uses for the box account JWT, so no random source is needed.
fn with_jti(mut claims: Value) -> Value {
    claims["jti"] = Value::String(String::new());
    let jti = BASE32_NOPAD.encode(&Sha256::digest(claims.to_string().as_bytes()));
    claims["jti"] = Value::String(jti);
    claims
}

pub fn operator_claims(keys: &PinnedKeys, system_account: &str, issued_at: i64) -> Value {
    with_jti(json!({
        "iat": issued_at,
        "iss": keys.root,
        "name": OPERATOR_NAME,
        "sub": keys.root,
        "nats": {
            "type": "operator",
            "version": 2,
            "system_account": system_account,
            "signing_keys": [keys.signer],
            "strict_signing_key_usage": false,
        },
    }))
}

pub fn system_account_claims(keys: &PinnedKeys, system_account: &str, issued_at: i64) -> Value {
    with_jti(json!({
        "iat": issued_at,
        "iss": keys.root,
        "name": SYSTEM_ACCOUNT_NAME,
        "sub": system_account,
        "nats": {
            "type": "account",
            "version": 2,
            "limits": {
                "subs": -1, "data": -1, "payload": -1, "imports": -1, "exports": -1,
                "wildcards": true, "conn": -1, "leaf": -1,
                "mem_storage": 0, "disk_storage": 0, "streams": 0, "consumer": 0,
            },
            "signing_keys": [keys.sysaccount],
            "default_permissions": {"pub": {}, "sub": {}},
        },
    }))
}

/// The exact ASCII the root signs: `<b64url header>.<b64url claims>`.
pub fn signing_input(claims: &Value) -> String {
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(JWT_HEADER.as_bytes()),
        URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes())
    )
}

/// Decodes a signing input and refuses unless re-encoding the decoded header and claims
/// reproduces `input` byte for byte. The claims shown for approval are the decoded ones,
/// so this is what makes "what was shown" and "what was signed" the same bytes: a second
/// JSON key, odd whitespace, an escape the encoder would not write, or non-canonical
/// base64 would all decode to claims that differ from, or hide part of, what is signed.
pub fn decode_canonical(input: &[u8]) -> Result<Value, String> {
    let text = std::str::from_utf8(input).map_err(|_| "the signing input is not ASCII")?;
    if !text.is_ascii() {
        return Err("the signing input is not ASCII".to_string());
    }
    let (header, claims) = text
        .split_once('.')
        .ok_or("the signing input is not <header>.<claims>")?;
    if claims.contains('.') {
        return Err("the signing input has more than two parts".to_string());
    }
    if header != URL_SAFE_NO_PAD.encode(JWT_HEADER.as_bytes()) {
        return Err(format!("the header is not exactly {JWT_HEADER}"));
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(claims)
        .map_err(|_| "the claims are not unpadded base64url")?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("the claims are not JSON: {error}"))?;
    if URL_SAFE_NO_PAD.encode(&bytes) != claims || value.to_string().as_bytes() != bytes.as_slice()
    {
        return Err(
            "re-encoding the decoded claims does not reproduce the signing input exactly"
                .to_string(),
        );
    }
    Ok(value)
}

/// The fields shown for approval.
pub fn summary(payload: Payload, claims: &Value) -> Value {
    let nats = &claims["nats"];
    let mut shown = json!({
        "iss": claims["iss"],
        "sub": claims["sub"],
        "name": claims["name"],
        "signing_keys": nats["signing_keys"],
        "iat": claims["iat"],
        "jti": claims["jti"],
    });
    if payload == Payload::Operator {
        shown["system_account"] = nats["system_account"].clone();
    }
    shown
}

/// Verifies an Ed25519 signature over `input` under the nkey `public`.
pub fn verify(public: &str, input: &[u8], signature: &[u8]) -> Result<(), String> {
    KeyPair::from_public_key(public)
        .map_err(|_| format!("{public} is not an nkey"))?
        .verify(input, signature)
        .map_err(|_| format!("the signature does not verify under {public}"))
}

/// A stored JWT's claims, after its signature verifies under the pinned root and its
/// signing input decodes canonically.
pub fn verify_stored(jwt: &str, root: &str) -> Result<Value, String> {
    let (input, signature) = jwt.trim().rsplit_once('.').ok_or("not a signed JWT")?;
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| "the signature is not unpadded base64url")?;
    verify(root, input.as_bytes(), &signature)?;
    decode_canonical(input.as_bytes())
}

/// The claim paths on which `stored` and `planned` differ, with `iat` and `jti` left
/// out: those change on every build, and re-signing only to refresh them would let a
/// fresher `iat` in the preload overwrite what the server stored (N4).
pub fn differing_claims(stored: &Value, planned: &Value) -> Vec<String> {
    let mut differs = Vec::new();
    let keys = |value: &Value| {
        value
            .as_object()
            .map(|map| {
                map.keys()
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>()
            })
            .unwrap_or_default()
    };
    for key in keys(stored).union(&keys(planned)) {
        if key == "iat" || key == "jti" {
            continue;
        }
        if key == "nats" {
            for inner in keys(&stored["nats"]).union(&keys(&planned["nats"])) {
                if stored["nats"][inner] != planned["nats"][inner] {
                    differs.push(format!("nats.{inner}"));
                }
            }
        } else if stored[key] != planned[key] {
            differs.push(key.clone());
        }
    }
    differs
}

/// Checks that a root-signed payload has the shape this install needs, so a valid
/// signature over some other root-signed JWT is not written in its place.
pub fn check_shape(
    payload: Payload,
    claims: &Value,
    root: &str,
    system_account: &str,
) -> Result<(), String> {
    let label = payload.label();
    if claims["iss"] != root {
        return Err(format!("the {label} payload's iss is not the pinned root"));
    }
    let (sub, kind) = match payload {
        Payload::Operator => (root, "operator"),
        Payload::SystemAccount => (system_account, "account"),
    };
    if claims["sub"] != sub {
        return Err(format!("the {label} payload's sub is not {sub}"));
    }
    if claims["nats"]["type"] != kind {
        return Err(format!("the {label} payload is not an {kind} JWT"));
    }
    if payload == Payload::Operator && claims["nats"]["system_account"] != system_account {
        return Err(format!(
            "the operator payload names system account {}, not {system_account}",
            claims["nats"]["system_account"]
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> PinnedKeys {
        PinnedKeys {
            root: KeyPair::new_operator().public_key(),
            signer: KeyPair::new_operator().public_key(),
            sysaccount: KeyPair::new_account().public_key(),
        }
    }

    #[test]
    fn built_inputs_decode_canonically() {
        let keys = keys();
        let sys = KeyPair::new_account().public_key();
        for claims in [
            operator_claims(&keys, &sys, 1_700_000_000),
            system_account_claims(&keys, &sys, 1_700_000_000),
        ] {
            let input = signing_input(&claims);
            assert_eq!(decode_canonical(input.as_bytes()).unwrap(), claims);
        }
    }

    /// Each input decodes to the same claims as the canonical one (or hides a second
    /// value for a key), so only the byte-exact re-encoding check can refuse it.
    #[test]
    fn claims_that_do_not_re_encode_exactly_are_refused() {
        let keys = keys();
        let sys = KeyPair::new_account().public_key();
        let canonical = operator_claims(&keys, &sys, 1_700_000_000).to_string();
        let header = URL_SAFE_NO_PAD.encode(JWT_HEADER.as_bytes());
        let variants = [
            // Whitespace the encoder never writes.
            canonical.replacen(':', ": ", 1),
            // A duplicated key: the decoder keeps the last value, the approver sees one.
            canonical.replacen('{', r#"{"name":"shown-but-overridden","#, 1),
            // An escape the encoder would not write.
            canonical.replacen("ckbus-operator", r"ckbus\u002doperator", 1),
        ];
        for variant in variants {
            let input = format!("{header}.{}", URL_SAFE_NO_PAD.encode(variant.as_bytes()));
            let error = decode_canonical(input.as_bytes())
                .expect_err(&format!("{variant} must be refused"));
            assert!(error.contains("re-encoding"), "{error}");
        }
        let error = decode_canonical(format!("{header}=.e30").as_bytes()).unwrap_err();
        assert!(error.contains("header"), "{error}");
    }

    #[test]
    fn differing_claims_ignore_iat_and_jti_only() {
        let keys = keys();
        let sys = KeyPair::new_account().public_key();
        let a = system_account_claims(&keys, &sys, 1);
        let b = system_account_claims(&keys, &sys, 2);
        assert_ne!(a["jti"], b["jti"]);
        assert!(differing_claims(&a, &b).is_empty());
        let rotated = PinnedKeys {
            sysaccount: KeyPair::new_account().public_key(),
            ..keys
        };
        assert_eq!(
            differing_claims(&a, &system_account_claims(&rotated, &sys, 1)),
            vec!["nats.signing_keys".to_string()]
        );
    }
}
