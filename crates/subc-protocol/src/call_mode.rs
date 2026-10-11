//! Call modes stamped by the daemon from the scope of a route.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::scope::ScopeStamp;

/// Declaring this in `capabilities.provides` promises that a module reads the
/// call mode on every bind and handles classic, codemode and flow calls.
pub const CALL_MODES_CAPABILITY: &str = "call-modes/v1";

/// The mode the daemon derives from a route's scope, never from its request body.
///
/// Providers must refuse calls whose mode is [`CallMode::Unknown`]; an unfamiliar
/// mode must never silently acquire classic behavior or delegated authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallMode {
    Classic,
    Codemode,
    Flow,
    Unknown(String),
}

impl Serialize for CallMode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::Classic => "classic",
            Self::Codemode => "codemode",
            Self::Flow => "flow",
            Self::Unknown(value) => value,
        })
    }
}

impl<'de> Deserialize<'de> for CallMode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "classic" => Self::Classic,
            "codemode" => Self::Codemode,
            "flow" => Self::Flow,
            value => Self::Unknown(value.to_owned()),
        })
    }
}

/// Derive the mode solely from the daemon's scope stamp. A run takes precedence
/// over a flow; a plain scope or no scope is classic. This also supplies the
/// fallback for readers receiving a bind from a daemon that predates call modes.
pub fn derive_call_mode(scope: Option<&ScopeStamp>) -> CallMode {
    match scope {
        Some(stamp) if stamp.attributes.run_id.is_some() => CallMode::Codemode,
        Some(stamp) if stamp.attributes.flow_id.is_some() => CallMode::Flow,
        _ => CallMode::Classic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        scope::{ScopeAttributes, ScopeKind},
        Principal,
    };

    fn stamp(attributes: ScopeAttributes) -> ScopeStamp {
        ScopeStamp::new(Principal::Direct, "scope-1", 1, ScopeKind::Ephemeral)
            .with_attributes(attributes)
    }

    #[test]
    fn known_modes_round_trip_as_lowercase_strings() {
        for (mode, wire) in [
            (CallMode::Classic, "\"classic\""),
            (CallMode::Codemode, "\"codemode\""),
            (CallMode::Flow, "\"flow\""),
        ] {
            assert_eq!(serde_json::to_string(&mode).unwrap(), wire);
            assert_eq!(serde_json::from_str::<CallMode>(wire).unwrap(), mode);
        }
    }

    #[test]
    fn unknown_modes_preserve_the_string_and_non_strings_are_refused() {
        for value in ["future-mode", "Classic", "", "mode\nwith\"escapes"] {
            let wire = serde_json::to_string(value).unwrap();
            let mode = serde_json::from_str::<CallMode>(&wire).unwrap();
            assert_eq!(mode, CallMode::Unknown(value.to_string()));
            assert_eq!(serde_json::to_string(&mode).unwrap(), wire);
        }
        for wire in ["null", "1", "true", "{}", "[]"] {
            assert!(serde_json::from_str::<CallMode>(wire).is_err());
        }
    }

    #[test]
    fn run_scope_derives_codemode_even_when_delegating() {
        for delegates in [false, true] {
            let stamp = stamp(
                ScopeAttributes::new()
                    .with_agent_id(Some("agent-1".to_string()))
                    .with_run_id(Some("run-1".to_string()))
                    .with_delegates(delegates),
            );
            assert_eq!(derive_call_mode(Some(&stamp)), CallMode::Codemode);
        }
    }

    #[test]
    fn flow_scope_derives_flow() {
        let stamp = stamp(ScopeAttributes::new().with_flow_id(Some("flow-1".to_string())));
        assert_eq!(derive_call_mode(Some(&stamp)), CallMode::Flow);
    }

    #[test]
    fn run_id_takes_precedence_over_flow_id() {
        let stamp = stamp(
            ScopeAttributes::new()
                .with_run_id(Some("run-1".to_string()))
                .with_flow_id(Some("flow-1".to_string())),
        );
        assert_eq!(derive_call_mode(Some(&stamp)), CallMode::Codemode);
    }

    #[test]
    fn plain_scope_and_no_scope_derive_classic() {
        let plain = stamp(ScopeAttributes::new());
        let agent = stamp(
            ScopeAttributes::new()
                .with_agent_id(Some("agent-1".to_string()))
                .with_delegates(true),
        );
        assert_eq!(derive_call_mode(Some(&plain)), CallMode::Classic);
        assert_eq!(derive_call_mode(Some(&agent)), CallMode::Classic);
        assert_eq!(derive_call_mode(None), CallMode::Classic);
    }
}
