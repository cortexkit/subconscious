//! Owner-only scope service history, separate from the exhaustive control enums.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const SCOPE_SERVED_BY_OP: &str = "scope.served_by";

/// Ask about an epoch owned by the registered module sending the request.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScopeServedByRequest {
    op: ScopeServedByOp,
    pub scope_ref: String,
    pub scope_epoch: u64,
    pub module_id: String,
}

impl ScopeServedByRequest {
    pub fn new(scope_ref: String, scope_epoch: u64, module_id: String) -> Self {
        Self {
            op: ScopeServedByOp::ServedBy,
            scope_ref,
            scope_epoch,
            module_id,
        }
    }
}

/// Only `Served` establishes that the target acknowledged a scoped bind.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeServedByAnswer {
    Served,
    NotServed,
    Unknown,
    Other(String),
}

impl Serialize for ScopeServedByAnswer {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::Served => "served",
            Self::NotServed => "not_served",
            Self::Unknown => "unknown",
            Self::Other(value) => value,
        })
    }
}

impl<'de> Deserialize<'de> for ScopeServedByAnswer {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "served" => Self::Served,
            "not_served" => Self::NotServed,
            "unknown" => Self::Unknown,
            other => Self::Other(other.to_owned()),
        })
    }
}

/// Volatile bind history in one daemon incarnation; unknown never means served.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScopeServedByReply {
    op: ScopeServedByOp,
    pub answer: ScopeServedByAnswer,
    pub daemon_incarnation: String,
}

impl ScopeServedByReply {
    pub fn new(answer: ScopeServedByAnswer, daemon_incarnation: String) -> Self {
        Self {
            op: ScopeServedByOp::ServedBy,
            answer,
            daemon_incarnation,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum ScopeServedByOp {
    #[serde(rename = "scope.served_by")]
    ServedBy,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn served_by_wire_is_open_but_requires_the_right_op() {
        let request = ScopeServedByRequest::new("head".into(), 7, "tool".into());
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({
                "op": "scope.served_by", "scope_ref": "head", "scope_epoch": 7, "module_id": "tool"
            })
        );
        for (wire, answer) in [
            ("served", ScopeServedByAnswer::Served),
            ("not_served", ScopeServedByAnswer::NotServed),
            ("unknown", ScopeServedByAnswer::Unknown),
            ("later", ScopeServedByAnswer::Other("later".into())),
        ] {
            let value = serde_json::json!({"op":"scope.served_by", "answer":wire, "daemon_incarnation":"d"});
            let reply: ScopeServedByReply = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(reply.answer, answer);
            assert_eq!(serde_json::to_value(reply).unwrap(), value);
        }
        assert!(serde_json::from_value::<ScopeServedByReply>(
            serde_json::json!({"op":"scope.describe", "answer":"served", "daemon_incarnation":"d"})
        )
        .is_err());
        assert!(serde_json::from_value::<ScopeServedByAnswer>(serde_json::json!(42)).is_err());
    }
}
