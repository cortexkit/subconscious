use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use subc_config::{canonical_json, raw_digest, Digest};
use subc_protocol::policy_root::{decode_public_key, fingerprint, key_id, PolicyRootPin};

use crate::config_gate::GateError;

pub(crate) mod ledger;

pub(crate) const SECTION: &str = "agent_policy_root";
pub(crate) const POINTER: &str = "/agent_policy_root";
const ROTATION: &str = "ck-policy-root-rotation/v1";
const RECOVERY: &str = "ck-policy-root-recovery/v1";

pub(crate) type Section = Result<Option<Key>, &'static str>;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Key {
    pub(crate) key_id: String,
    pub(crate) public_key_hex: String,
}

impl Key {
    fn checked(&self) -> Result<VerifyingKey, &'static str> {
        let raw = decode_public_key(&self.public_key_hex).ok_or("policy_root_not_canonical")?;
        if !canonical_hex(&self.key_id, 16) {
            return Err("policy_root_not_canonical");
        }
        if key_id(&raw) != self.key_id {
            return Err("policy_root_key_id_mismatch");
        }
        VerifyingKey::from_bytes(&raw)
            .ok()
            .filter(|key| !key.is_weak() && key.to_edwards().compress().to_bytes() == raw)
            .ok_or("policy_root_not_canonical")
    }

    pub(crate) fn pin(self, retired_key_ids: Vec<String>) -> PolicyRootPin {
        PolicyRootPin {
            key_id: self.key_id,
            public_key_hex: self.public_key_hex,
            retired_key_ids,
        }
    }

    fn fingerprint(&self) -> String {
        fingerprint(&decode_public_key(&self.public_key_hex).expect("validated public key"))
    }
}

pub(crate) fn present_section<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(deserializer).map(Some)
}

pub(crate) fn parse_section(raw: Option<serde_json::Value>) -> Section {
    let Some(raw) = raw else { return Ok(None) };
    let key: Key = serde_json::from_value(raw).map_err(|_| "policy_root_not_canonical")?;
    key.checked()?;
    Ok(Some(key))
}

fn canonical_hex(text: &str, length: usize) -> bool {
    text.len() == length
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rotation {
    v: String,
    from_key_id: String,
    to_key_id: String,
    to_public_key_hex: String,
    #[serde(rename = "issued_at_unix_secs")]
    _issued_at_unix_secs: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Recovery {
    v: String,
    retired_key_id: String,
    to_key_id: String,
    to_public_key_hex: String,
    #[serde(rename = "issued_at_unix_secs")]
    _issued_at_unix_secs: u64,
}

pub(crate) enum Move {
    Set(Key),
    Rotate(subc_protocol::policy_root::RotatePolicyRoot),
    Recover(subc_control::policy_root::RecoverPolicyRoot),
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Change {
    pub(crate) next: Key,
    pub(crate) previous: Option<String>,
    pub(crate) departure: Option<ledger::Departure>,
    pub(crate) statement_digest: Option<Digest>,
    pub(crate) review: String,
}

impl Move {
    pub(crate) fn check(
        self,
        current: Option<&Key>,
        ledger: &ledger::Ledger,
    ) -> Result<Change, GateError> {
        let (next, departure, statement_digest) = match self {
            Self::Set(key) => {
                if current.is_some() {
                    return Err(refuse("policy_root_pin_exists"));
                }
                (key, None, None)
            }
            Self::Rotate(request) => {
                let current = current.ok_or_else(|| refuse("policy_root_absent"))?;
                let (bytes, body): (_, Rotation) = canonical_body(&request.statement_b64)?;
                if body.v != ROTATION {
                    return Err(refuse("policy_root_not_canonical"));
                }
                if body.from_key_id != current.key_id {
                    return Err(refuse("policy_root_wrong_from_key"));
                }
                verify(current, &bytes, &request.signature_hex)?;
                (
                    Key {
                        key_id: body.to_key_id,
                        public_key_hex: body.to_public_key_hex,
                    },
                    Some(ledger::Departure::Rotated),
                    Some(raw_digest(&bytes)),
                )
            }
            Self::Recover(request) => {
                let current = current.ok_or_else(|| refuse("policy_root_absent"))?;
                let (bytes, body): (_, Recovery) = canonical_body(&request.body_b64)?;
                if body.v != RECOVERY {
                    return Err(refuse("policy_root_not_canonical"));
                }
                if body.retired_key_id != current.key_id {
                    return Err(refuse("policy_root_wrong_from_key"));
                }
                (
                    Key {
                        key_id: body.to_key_id,
                        public_key_hex: body.to_public_key_hex,
                    },
                    Some(ledger::Departure::Retired),
                    Some(raw_digest(&bytes)),
                )
            }
        };
        next.checked().map_err(refuse)?;
        if current.is_some_and(|key| key.key_id == next.key_id) || ledger.contains(&next.key_id) {
            return Err(refuse("policy_root_key_reused"));
        }
        let review = match (current, departure) {
            (None, _) => format!("Set the agent policy trust root: {}", next.fingerprint()),
            (Some(old), reason) => {
                let heading = if reason == Some(ledger::Departure::Retired) {
                    "This replaces the trust root without the old root's consent. Use it only if the old root is lost or leaked."
                } else {
                    "Replace the agent policy trust root."
                };
                format!(
                    "{heading} Old: {}. New: {}.",
                    old.fingerprint(),
                    next.fingerprint()
                )
            }
        };
        Ok(Change {
            next,
            previous: current.map(|old| old.key_id.clone()),
            departure,
            statement_digest,
            review,
        })
    }
}

fn canonical_body<T: serde::de::DeserializeOwned>(text: &str) -> Result<(Vec<u8>, T), GateError> {
    use base64::{engine::general_purpose::STANDARD, Engine};

    let invalid = || refuse("policy_root_not_canonical");
    // Both statement bodies (rotation and recovery) hold only two key ids, one public key,
    // a version tag and an integer timestamp, so 1 KiB of base64 bounds either of them.
    if text.len() > 1024 {
        return Err(invalid());
    }
    let body = STANDARD.decode(text).map_err(|_| invalid())?;
    let typed = serde_json::from_slice(&body).map_err(|_| invalid())?;
    let value = serde_json::from_slice(&body).map_err(|_| invalid())?;
    if canonical_json(&value).map_err(|_| invalid())? != body {
        return Err(invalid());
    }
    Ok((body, typed))
}

fn verify(current: &Key, body: &[u8], signature_hex: &str) -> Result<(), GateError> {
    let invalid = || refuse("policy_root_signature_invalid");
    if !canonical_hex(signature_hex, 128) {
        return Err(invalid());
    }
    let mut signature = [0; 64];
    for (i, byte) in signature.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&signature_hex[2 * i..2 * i + 2], 16).map_err(|_| invalid())?;
    }
    let signed = [ROTATION.as_bytes(), b"\0", body].concat();
    current
        .checked()
        .map_err(refuse)?
        .verify_strict(&signed, &Signature::from_bytes(&signature))
        .map_err(|_| invalid())
}

pub(crate) fn refuse(code: &'static str) -> GateError {
    let detail = match code {
        "policy_root_absent" => "no agent policy trust root is pinned",
        "policy_root_signature_invalid" => {
            "the rotation signature does not verify under the current pin"
        }
        "policy_root_not_canonical" => {
            "the policy root input is not canonical or has an invalid shape"
        }
        "policy_root_wrong_from_key" => "the statement does not replace the current pin",
        "policy_root_key_id_mismatch" => "the key id does not derive from the public key",
        "policy_root_key_reused" => "the proposed key is current or has previously left the pin",
        "policy_root_pin_exists" => "the daemon already has a valid agent policy trust root",
        "policy_root_change_pending" => "another agent policy trust root change is pending",
        "policy_root_caller_not_permitted" => {
            "this connection may not perform the requested policy root operation"
        }
        _ => "the agent policy root was refused",
    };
    GateError::new(code, detail)
}
