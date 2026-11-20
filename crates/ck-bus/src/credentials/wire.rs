//! Claustrum's `credential.sign` and `credential.public_key`, byte for byte.
//!
//! Read at claustrum `57a501b`: `crates/credentials-module/src/main.rs::ReadRequest` (the
//! `{method, params}` request body), `::wrap_result` (every reply is `{"result": ...}`,
//! and a refusal is `{"result": {"error": {code, class}}}` in an ordinary response),
//! `crates/credentials-module/src/read_surface.rs::SignParams`, `::SignResult`,
//! `::PublicKeyParams` and `::PublicKeyResult`, and
//! `crates/credentials-core/src/signing.rs::sign_ed25519` and `::key_id_for_public`.
//!
//! The request's `payload_b64` is standard base64 and the vault signs the DECODED bytes
//! with pure Ed25519 (no pre-hash). The reply's `signature_b64` is standard, padded base64
//! of the 64 signature bytes, and `key_id` is lowercase hex of the first 8 bytes of
//! SHA-256 over the 32 raw public bytes. The parsers here accept exactly that and refuse
//! anything else by naming the field, so a signer that drifts in alphabet, padding or
//! length is caught at the first reply rather than as a broker authorization failure.

use std::fmt;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const SIGN_METHOD: &str = "credential.sign";
pub const PUBLIC_KEY_METHOD: &str = "credential.public_key";

/// The largest payload the vault signs in one request (`signing.rs::MAX_SIGN_PAYLOAD`).
/// ck-bus refuses a larger payload before sending it.
pub const MAX_SIGN_PAYLOAD: usize = 1024 * 1024;

/// A detached signature and the id of the key that made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultSignature {
    pub signature: [u8; 64],
    pub key_id: String,
}

/// The public half of a vault signing key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultPublicKey {
    pub public: [u8; 32],
    pub key_id: String,
}

/// Why a vault reply could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyError {
    /// The vault answered with its refusal body.
    Refused { code: String, class: Option<String> },
    /// The reply is not in the recorded shape; the message names the field.
    Malformed(String),
}

impl fmt::Display for ReplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused { code, class } => match class {
                Some(class) => write!(f, "vault refused: {code} ({class})"),
                None => write!(f, "vault refused: {code}"),
            },
            Self::Malformed(detail) => write!(f, "vault reply malformed: {detail}"),
        }
    }
}

/// The request body for `credential.sign` by credential id. Refuses a payload over the
/// vault's cap rather than sending one the vault will refuse.
pub fn sign_request(credential_id: &str, payload: &[u8]) -> Result<Vec<u8>, ReplyError> {
    if payload.len() > MAX_SIGN_PAYLOAD {
        return Err(ReplyError::Malformed(format!(
            "payload of {} bytes exceeds the vault's {MAX_SIGN_PAYLOAD}-byte signing cap",
            payload.len()
        )));
    }
    Ok(encode(json!({
        "method": SIGN_METHOD,
        "params": {
            "credential_id": credential_id,
            "payload_b64": STANDARD.encode(payload),
        },
    })))
}

/// The request body for `credential.public_key` by credential id.
pub fn public_key_request(credential_id: &str) -> Vec<u8> {
    encode(json!({
        "method": PUBLIC_KEY_METHOD,
        "params": { "credential_id": credential_id },
    }))
}

pub(super) const BUS_SEAL_ID: &str = "kem:bus-seal:1";

pub(super) fn open_bus_seal_request(enc: &[u8], ciphertext: &[u8], info: &[u8]) -> Vec<u8> {
    encode(json!({
        "method": "credential.open",
        "params": {
            "credential_id": BUS_SEAL_ID,
            "enc_b64": STANDARD.encode(enc),
            "ciphertext_b64": STANDARD.encode(ciphertext),
            "info_b64": STANDARD.encode(info),
            "aad_b64": "",
        },
    }))
}

pub(super) fn parse_open_reply(body: &[u8]) -> Result<Vec<u8>, ReplyError> {
    let result = result_object(body)?;
    let _ = string_field(&result, "key_id")?;
    STANDARD
        .decode(string_field(&result, "plaintext_b64")?)
        .map_err(|error| {
            ReplyError::Malformed(format!(
                "plaintext_b64 is not standard padded base64: {error}"
            ))
        })
}

#[cfg(test)]
#[test]
fn handoff_vault_open_uses_exact_params_and_strict_base64() {
    let shape: Value = serde_json::from_slice(&open_bus_seal_request(
        &[0xff],
        &[0x00, 0xff],
        b"ck-bus-seal/v1/ckbus/a|b",
    ))
    .unwrap();
    assert_eq!(
        shape,
        json!({
            "method": "credential.open",
            "params": {
                "credential_id": "kem:bus-seal:1",
                "enc_b64": "/w==",
                "ciphertext_b64": "AP8=",
                "info_b64": "Y2stYnVzLXNlYWwvdjEvY2tidXMvYXxi",
                "aad_b64": "",
            },
        })
    );
    assert_eq!(
        parse_open_reply(br#"{"result":{"plaintext_b64":"AP8=","key_id":"fixture"}}"#).unwrap(),
        [0x00, 0xff],
    );
    assert!(parse_open_reply(br#"{"result":{"plaintext_b64":"AP8","key_id":"fixture"}}"#).is_err());
    assert!(matches!(
        parse_open_reply(br#"{"result":{"error":{"code":"open_failed","class":"permanent"}}}"#),
        Err(ReplyError::Refused { code, .. }) if code == "open_failed",
    ));
}

fn encode(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).expect("a JSON value always encodes")
}

/// `key_id` as the vault derives it: lowercase hex of `sha256(public)[..8]`.
pub fn key_id_for(public: &[u8; 32]) -> String {
    hex_lower(&Sha256::digest(public)[..8])
}

pub fn parse_sign_reply(body: &[u8]) -> Result<VaultSignature, ReplyError> {
    let result = result_object(body)?;
    let signature_b64 = string_field(&result, "signature_b64")?;
    // The standard engine requires canonical padding, so base64url and unpadded text are
    // both refused here, not only a wrong length.
    let bytes = STANDARD.decode(signature_b64).map_err(|error| {
        ReplyError::Malformed(format!(
            "signature_b64 is not standard padded base64: {error}"
        ))
    })?;
    let signature: [u8; 64] = bytes.as_slice().try_into().map_err(|_| {
        ReplyError::Malformed(format!(
            "signature_b64 decodes to {} bytes, not 64",
            bytes.len()
        ))
    })?;
    let key_id = key_id_field(&result)?;
    Ok(VaultSignature { signature, key_id })
}

pub fn parse_public_key_reply(body: &[u8]) -> Result<VaultPublicKey, ReplyError> {
    let result = result_object(body)?;
    let hex = string_field(&result, "public_key_hex")?;
    let public: [u8; 32] = decode_hex_lower(hex)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| {
            ReplyError::Malformed("public_key_hex is not 32 bytes of lowercase hex".to_string())
        })?;
    let algorithm = string_field(&result, "algorithm")?;
    if algorithm != "ed25519" {
        return Err(ReplyError::Malformed(format!(
            "algorithm is {algorithm:?}, not \"ed25519\""
        )));
    }
    let key_id = key_id_field(&result)?;
    if key_id != key_id_for(&public) {
        return Err(ReplyError::Malformed(format!(
            "key_id {key_id} is not sha256(public_key_hex)[..8]"
        )));
    }
    Ok(VaultPublicKey { public, key_id })
}

fn result_object(body: &[u8]) -> Result<serde_json::Map<String, Value>, ReplyError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| ReplyError::Malformed(format!("reply is not JSON: {error}")))?;
    let Some(result) = value.get("result").and_then(Value::as_object) else {
        return Err(ReplyError::Malformed(
            "reply has no \"result\" object".to_string(),
        ));
    };
    if let Some(error) = result.get("error") {
        let code = error
            .get("code")
            .and_then(Value::as_str)
            .ok_or_else(|| ReplyError::Malformed("refusal has no string code".to_string()))?;
        return Err(ReplyError::Refused {
            code: code.to_string(),
            class: error
                .get("class")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    Ok(result.clone())
}

fn string_field<'a>(
    object: &'a serde_json::Map<String, Value>,
    name: &str,
) -> Result<&'a str, ReplyError> {
    object
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| ReplyError::Malformed(format!("reply has no string field {name}")))
}

fn key_id_field(object: &serde_json::Map<String, Value>) -> Result<String, ReplyError> {
    let key_id = string_field(object, "key_id")?;
    if key_id.len() != 16 || decode_hex_lower(key_id).is_none() {
        return Err(ReplyError::Malformed(format!(
            "key_id {key_id:?} is not 8 bytes of lowercase hex"
        )));
    }
    Ok(key_id.to_string())
}

pub fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Decodes lowercase hex only; an uppercase digit is a shape divergence, not an
/// equivalent spelling.
pub fn decode_hex_lower(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    text.as_bytes()
        .chunks(2)
        .map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect()
}
