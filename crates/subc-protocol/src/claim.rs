//! Single-use claims for connections belonging to a supervised spawn.
//!
//! These standalone bodies use channel 0, the connection's control lane. Merely
//! defining them does not advertise daemon support for either operation.
//! Success uses a RESPONSE; refusals use [`crate::ErrorBody`] and the codes in
//! [`crate::error_codes`]. Claims grant nothing until the daemon consumes them.

use std::{collections::BTreeMap, fmt};

use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// An opaque bearer claim. Debug output is always redacted, including when a
/// containing request or reply is formatted. Never log the serialized body or
/// exposed value, or put the claim in argv or an environment variable.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Claim(String);

impl Claim {
    /// Wrap an opaque wire value. Only the daemon validates and consumes claims;
    /// constructing this value does not establish a connection's identity.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Access the bearer value for an explicit handoff, never for logging.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Claim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Claim([REDACTED])")
    }
}

/// The authority requested for one additional connection. Only a prefix claim
/// carries a module id. The daemon checks its grammar and that the minting
/// module owns the longest matching reserved prefix, excluding exact reserved
/// or gated ids and prefixes without an owner.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "purpose")]
pub enum ClaimPurpose {
    #[serde(rename = "self")]
    SelfConnection,
    #[serde(rename = "helper")]
    Helper,
    #[serde(rename = "relay")]
    Relay,
    #[serde(rename = "prefix")]
    Prefix { module_id: String },
}

impl<'de> Deserialize<'de> for ClaimPurpose {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Purpose {
            #[serde(rename = "self")]
            SelfConnection,
            Helper,
            Relay,
            Prefix,
        }
        #[derive(Deserialize)]
        struct Wire {
            purpose: Purpose,
            #[serde(flatten)]
            fields: BTreeMap<String, Value>,
        }
        let mut wire = Wire::deserialize(deserializer)?;
        let module_id = wire.fields.remove("module_id");
        match wire.purpose {
            Purpose::Prefix => match module_id {
                Some(Value::String(module_id)) => Ok(Self::Prefix { module_id }),
                _ => Err(D::Error::custom("prefix claims require a string module_id")),
            },
            _ if module_id.is_some() => {
                Err(D::Error::custom("module_id is only for prefix claims"))
            }
            Purpose::SelfConnection => Ok(Self::SelfConnection),
            Purpose::Helper => Ok(Self::Helper),
            Purpose::Relay => Ok(Self::Relay),
        }
    }
}

/// Mint only over the spawn's live primary registration channel. Claim-bound
/// connections cannot mint. Refusals never permit falling back to direct.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimMintRequest {
    op: MintOp,
    #[serde(flatten)]
    pub purpose: ClaimPurpose,
}

impl ClaimMintRequest {
    pub fn new(purpose: ClaimPurpose) -> Self {
        Self {
            op: MintOp::Op,
            purpose,
        }
    }
}

/// One single-use 128-bit base64url claim, redeemable for 30 seconds on the
/// daemon's monotonic clock. The wall-clock expiry is informational only;
/// readers must not use it to decide that a claim is still valid.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimMintReply {
    op: MintOp,
    pub claim: Claim,
    pub expires_at_ms: u64,
}

impl ClaimMintReply {
    pub fn new(claim: Claim, expires_at_ms: u64) -> Self {
        Self {
            op: MintOp::Op,
            claim,
            expires_at_ms,
        }
    }
}

/// The first frame after a new connection's handshake. Acceptance consumes the
/// claim and binds the connection atomically while rechecking the live spawn.
/// Any refused presentation closes the connection, including a retryable
/// refusal; it never continues as direct.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimPresentRequest {
    op: PresentOp,
    pub claim: Claim,
}

impl ClaimPresentRequest {
    pub fn new(claim: Claim) -> Self {
        Self {
            op: PresentOp::Op,
            claim,
        }
    }
}

/// Acceptance of a single-use claim. Refusals use an ERROR frame, never this ACK.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimPresentReply {
    op: PresentOp,
}

impl ClaimPresentReply {
    pub fn new() -> Self {
        Self { op: PresentOp::Op }
    }
}

impl Default for ClaimPresentReply {
    fn default() -> Self {
        Self::new()
    }
}

/// Backoff carried in [`crate::ErrorBody::detail`] for `claim_rate_limited`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimRateLimitedDetail {
    pub retry_after_ms: u64,
}

impl ClaimRateLimitedDetail {
    pub fn new(retry_after_ms: u64) -> Self {
        Self { retry_after_ms }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum MintOp {
    #[serde(rename = "claim.mint")]
    Op,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum PresentOp {
    #[serde(rename = "claim.present")]
    Op,
}

#[cfg(test)]
mod claim_wire_tests {
    use super::*;
    use crate::{error_codes, manifest::ModuleManifest, ErrorBody};
    use serde_json::json;

    macro_rules! purpose_test {
        ($name:ident, $purpose:expr, $body:expr) => {
            #[test]
            fn $name() {
                let expected = $body;
                let request = ClaimMintRequest::new($purpose);
                assert_eq!(serde_json::to_value(&request).unwrap(), expected);
                assert_eq!(
                    serde_json::from_value::<ClaimMintRequest>(expected).unwrap(),
                    request
                );
            }
        };
    }
    purpose_test!(
        mint_self,
        ClaimPurpose::SelfConnection,
        json!({"op":"claim.mint","purpose":"self"})
    );
    purpose_test!(
        mint_helper,
        ClaimPurpose::Helper,
        json!({"op":"claim.mint","purpose":"helper"})
    );
    purpose_test!(
        mint_relay,
        ClaimPurpose::Relay,
        json!({"op":"claim.mint","purpose":"relay"})
    );
    purpose_test!(
        mint_prefix,
        ClaimPurpose::Prefix {
            module_id: "worker.child".into()
        },
        json!({"op":"claim.mint","purpose":"prefix","module_id":"worker.child"})
    );

    #[test]
    fn module_id_only_for_prefix_and_required_op_tags() {
        for purpose in ["self", "helper", "relay"] {
            for module_id in [json!("child"), Value::Null] {
                assert!(serde_json::from_value::<ClaimMintRequest>(json!({
                    "op":"claim.mint", "purpose":purpose, "module_id":module_id
                }))
                .is_err());
            }
        }
        for body in [
            json!({"op":"claim.mint","purpose":"prefix"}),
            json!({"op":"claim.mint","purpose":"prefix","module_id":5}),
            json!({"op":"claim.mint","purpose":"future"}),
            json!({"op":"claim.present","purpose":"self"}),
            json!({"purpose":"self"}),
        ] {
            assert!(serde_json::from_value::<ClaimMintRequest>(body).is_err());
        }
        assert!(
            serde_json::from_value::<ClaimPresentRequest>(json!({"op":"claim.present"})).is_err()
        );
        assert!(serde_json::from_value::<ClaimPresentReply>(json!({})).is_err());
        assert!(serde_json::from_value::<ClaimPresentReply>(json!({"op":"claim.mint"})).is_err());
        assert!(serde_json::from_value::<ClaimMintReply>(json!({"op":"claim.mint"})).is_err());
    }

    #[test]
    fn replies_and_presentation_round_trip() {
        let claim = Claim::new("AAAAAAAAAAAAAAAAAAAAAA");
        let reply = ClaimMintReply::new(claim.clone(), 1_700_000_030_000);
        let wire = json!({"op":"claim.mint","claim":claim,"expires_at_ms":1_700_000_030_000_u64});
        assert!(serde_json::to_value(&reply).unwrap() == wire);
        assert_eq!(
            serde_json::from_value::<ClaimMintReply>(wire).unwrap(),
            reply
        );
        let request = ClaimPresentRequest::new(claim.clone());
        let wire = json!({"op":"claim.present","claim":claim});
        assert!(serde_json::to_value(&request).unwrap() == wire);
        assert_eq!(
            serde_json::from_value::<ClaimPresentRequest>(wire).unwrap(),
            request
        );
        let ack = ClaimPresentReply::new();
        assert_eq!(
            serde_json::to_value(&ack).unwrap(),
            json!({"op":"claim.present"})
        );
        assert_eq!(
            serde_json::from_str::<ClaimPresentReply>(r#"{"op":"claim.present"}"#).unwrap(),
            ack
        );
    }

    #[test]
    fn debug_redacts_claim_and_containing_bodies() {
        let secret = "BBBBBBBBBBBBBBBBBBBBBB";
        let claim = Claim::new(secret);
        let decoded: Claim = serde_json::from_value(json!(secret)).unwrap();
        assert_eq!(decoded, claim);
        assert!(decoded.expose_secret() == secret);
        for debug in [
            format!("{claim:?}"),
            format!("{:?}", ClaimMintReply::new(claim.clone(), 0)),
            format!("{:?}", ClaimPresentRequest::new(claim)),
        ] {
            assert!(!debug.contains(secret));
            assert!(debug.contains("[REDACTED]"));
        }
    }

    macro_rules! refusal_test {
        ($name:ident, $code:ident, $spelling:literal, $retryable:expr) => {
            #[test]
            fn $name() {
                assert_eq!(error_codes::$code, $spelling);
                let wire = json!({"code":$spelling,"message":"claim refused"});
                let error: ErrorBody = serde_json::from_value(wire.clone()).unwrap();
                assert_eq!(serde_json::to_value(&error).unwrap(), wire);
                assert_eq!(error_codes::is_retryable_claim(&error.code), $retryable);
                assert!(!error_codes::is_retryable_route_open(&error.code));
                let table: Value = serde_json::from_str(include_str!("../tests/golden/decision_tables.json")).unwrap();
                assert_eq!(table["claim_retryable"][$spelling], if $retryable { "retryable" } else { "terminal" });
            }
        };
    }
    refusal_test!(
        mint_not_allowed,
        CLAIM_MINT_NOT_ALLOWED,
        "claim_mint_not_allowed",
        false
    );
    refusal_test!(
        unredeemed_limit,
        CLAIM_UNREDEEMED_LIMIT,
        "claim_unredeemed_limit",
        true
    );
    refusal_test!(rate_limited, CLAIM_RATE_LIMITED, "claim_rate_limited", true);
    refusal_test!(
        connection_limit,
        CLAIM_CONNECTION_LIMIT,
        "claim_connection_limit",
        true
    );
    refusal_test!(invalid, CLAIM_INVALID, "claim_invalid", false);
    refusal_test!(
        present_not_first,
        CLAIM_PRESENT_NOT_FIRST,
        "claim_present_not_first",
        false
    );
    refusal_test!(
        prefix_refused,
        CLAIM_PREFIX_REFUSED,
        "claim_prefix_refused",
        false
    );

    #[test]
    fn rate_limit_backoff_detail_round_trip() {
        let wire = json!({"code":"claim_rate_limited","message":"claim refused","detail":{"retry_after_ms":125}});
        let error: ErrorBody = serde_json::from_value(wire.clone()).unwrap();
        let detail: ClaimRateLimitedDetail =
            serde_json::from_value(error.detail.clone().unwrap()).unwrap();
        assert_eq!(detail, ClaimRateLimitedDetail::new(125));
        let encoded = ErrorBody::new(error_codes::CLAIM_RATE_LIMITED, "claim refused")
            .with_detail(serde_json::to_value(detail).unwrap());
        assert_eq!(serde_json::to_value(encoded).unwrap(), wire);
    }

    #[test]
    fn unknown_refusal_is_terminal() {
        assert!(!error_codes::is_retryable_claim("a_code_nobody_declared"));
    }

    #[test]
    fn manifest_registration_channel_is_optional_on_read() {
        let legacy = ModuleManifest::builder("worker", "1.0.0").build();
        let legacy_wire = serde_json::to_value(&legacy).unwrap();
        assert!(legacy_wire.get("registration_channel").is_none());
        let read: ModuleManifest = serde_json::from_value(legacy_wire.clone()).unwrap();
        assert_eq!(read.registration_channel, None);
        for supported in [true, false] {
            let mut wire = legacy_wire.clone();
            wire["registration_channel"] = json!(supported);
            let read: ModuleManifest = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(read.registration_channel, Some(supported));
            assert_eq!(serde_json::to_value(&read).unwrap(), wire);
            assert_eq!(
                ModuleManifest::builder("worker", "1.0.0")
                    .registration_channel(supported)
                    .build(),
                read
            );
        }
    }
}
