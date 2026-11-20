//! Channel-0 bodies for reading and moving the agent policy trust root.

use serde::{Deserialize, Serialize};
use subc_config::{raw_digest, Digest};

pub const GET_OP: &str = "policy_root.get";
pub const ROTATE_OP: &str = "policy_root.rotate";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetPolicyRoot {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RotatePolicyRoot {
    pub statement_b64: String,
    pub signature_hex: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyRootPin {
    pub key_id: String,
    pub public_key_hex: String,
    pub retired_key_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyRoot {
    pub pin: Option<PolicyRootPin>,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyRootChangeState {
    Pending,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyRootChange {
    pub digest: Digest,
    pub state: PolicyRootChangeState,
}

/// Decode a raw Ed25519 public key in its exact lowercase wire spelling.
pub fn decode_public_key(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
    {
        return None;
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

/// The key id uses the first eight bytes of SHA-256 of the raw public key.
pub fn key_id(public_key: &[u8; 32]) -> String {
    raw_digest(public_key).as_str()[7..23].to_owned()
}

/// The approval fingerprint uses sixteen hash bytes, grouped for comparison.
pub fn fingerprint(public_key: &[u8; 32]) -> String {
    let hash = raw_digest(public_key);
    let hex = &hash.as_str()[7..39];
    (0..8)
        .map(|i| &hex[i * 4..i * 4 + 4])
        .collect::<Vec<_>>()
        .join(" ")
}
