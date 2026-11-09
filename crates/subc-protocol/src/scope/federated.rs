use serde::{Deserialize, Serialize};

/// The paired peer's class, attested by the configured federated owner.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FederatedPeerClass {
    Computer,
    Phone,
    Sim,
    Unknown,
}

/// Identity asserted by the paired stack, not a local agent identity.
/// Optional names are display-only. Use [`RemoteClaim::same_identity`] to compare
/// callers without treating a display rename as a change of identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum RemoteClaim {
    /// Construct with [`RemoteClaim::agent`] so future fields can be added.
    /// ```compile_fail
    /// use subc_protocol::scope::RemoteClaim;
    /// let _ = RemoteClaim::Agent { agent_id: "agent".into() };
    /// ```
    #[non_exhaustive]
    Agent {
        agent_id: String,
        #[serde(
            default,
            deserialize_with = "deserialize_name",
            skip_serializing_if = "Option::is_none"
        )]
        agent_name: Option<String>,
    },
    /// Construct with [`RemoteClaim::worker`] so future fields can be added.
    /// ```compile_fail
    /// use subc_protocol::scope::RemoteClaim;
    /// let _ = RemoteClaim::Worker { owner_module_id: "owner".into(), worker_ref: "a".repeat(64), on_behalf_of: None };
    /// ```
    #[non_exhaustive]
    Worker {
        owner_module_id: String,
        worker_ref: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on_behalf_of: Option<String>,
        /// Display name of the agent identified by `on_behalf_of`, not the worker.
        #[serde(
            default,
            deserialize_with = "deserialize_name",
            skip_serializing_if = "Option::is_none"
        )]
        agent_name: Option<String>,
    },
    /// Construct with [`RemoteClaim::run`] so future fields can be added.
    /// ```compile_fail
    /// use subc_protocol::scope::RemoteClaim;
    /// let _ = RemoteClaim::Run { agent_id: "agent".into() };
    /// ```
    #[non_exhaustive]
    Run {
        agent_id: String,
        #[serde(
            default,
            deserialize_with = "deserialize_name",
            skip_serializing_if = "Option::is_none"
        )]
        agent_name: Option<String>,
    },
    /// Construct with [`RemoteClaim::flow`] so future fields can be added.
    /// ```compile_fail
    /// use subc_protocol::scope::RemoteClaim;
    /// let _ = RemoteClaim::Flow { flow_id: "flow".into(), flow_version: 1, flow_install_id: "install".into(), flow_code_hash: "a".repeat(64) };
    /// ```
    #[non_exhaustive]
    Flow {
        flow_id: String,
        flow_version: u64,
        flow_install_id: String,
        flow_code_hash: String,
        #[serde(
            default,
            deserialize_with = "deserialize_name",
            skip_serializing_if = "Option::is_none"
        )]
        flow_name: Option<String>,
    },
}

impl RemoteClaim {
    /// Whether these claims identify the same caller. Compares every field except
    /// the display names, so a field added later counts as identity unless it is
    /// cleared here too. Ordinary equality remains structural.
    pub fn same_identity(&self, other: &Self) -> bool {
        self.without_names() == other.without_names()
    }

    fn without_names(&self) -> Self {
        let mut claim = self.clone();
        match &mut claim {
            Self::Agent { agent_name, .. }
            | Self::Run { agent_name, .. }
            | Self::Worker { agent_name, .. } => *agent_name = None,
            Self::Flow { flow_name, .. } => *flow_name = None,
        }
        claim
    }

    pub fn agent(agent_id: impl Into<String>) -> Self {
        Self::Agent {
            agent_id: agent_id.into(),
            agent_name: None,
        }
    }

    pub fn worker(
        owner_module_id: impl Into<String>,
        worker_ref: impl Into<String>,
        on_behalf_of: Option<String>,
    ) -> Self {
        Self::Worker {
            owner_module_id: owner_module_id.into(),
            worker_ref: worker_ref.into(),
            on_behalf_of,
            agent_name: None,
        }
    }

    pub fn run(agent_id: impl Into<String>) -> Self {
        Self::Run {
            agent_id: agent_id.into(),
            agent_name: None,
        }
    }

    pub fn flow(
        flow_id: impl Into<String>,
        flow_version: u64,
        flow_install_id: impl Into<String>,
        flow_code_hash: impl Into<String>,
    ) -> Self {
        Self::Flow {
            flow_id: flow_id.into(),
            flow_version,
            flow_install_id: flow_install_id.into(),
            flow_code_hash: flow_code_hash.into(),
            flow_name: None,
        }
    }

    /// Set the claimed agent's display name (or the worker's `on_behalf_of` agent).
    /// Invalid names are dropped. Has no effect on flow claims.
    #[must_use]
    pub fn with_agent_name(mut self, name: impl Into<String>) -> Self {
        match &mut self {
            Self::Agent { agent_name, .. }
            | Self::Run { agent_name, .. }
            | Self::Worker { agent_name, .. } => *agent_name = valid_name(name.into()),
            Self::Flow { .. } => {}
        }
        self
    }

    /// Set the flow's display name. Invalid names are dropped.
    /// Has no effect on agent, run or worker claims.
    #[must_use]
    pub fn with_flow_name(mut self, name: impl Into<String>) -> Self {
        if let Self::Flow { flow_name, .. } = &mut self {
            *flow_name = valid_name(name.into());
        }
        self
    }

    /// Check identity fields, including the shared opaque-token bounds.
    /// Display names never invalidate a claim; decoders drop invalid names.
    pub fn validate(&self) -> Result<(), String> {
        let token = |name, value: &str| {
            crate::tool_call::validate_opaque_field(name, value).map_err(|e| e.to_string())
        };
        match self {
            Self::Agent { agent_id, .. } | Self::Run { agent_id, .. } => {
                token("agent_id", agent_id)
            }
            Self::Worker {
                owner_module_id,
                worker_ref,
                on_behalf_of,
                ..
            } => {
                crate::manifest::validate_module_id(owner_module_id).map_err(|e| e.to_string())?;
                hex("worker_ref", worker_ref, 64)?;
                if let Some(agent) = on_behalf_of {
                    token("on_behalf_of", agent)?;
                }
                Ok(())
            }
            Self::Flow {
                flow_id,
                flow_version,
                flow_install_id,
                flow_code_hash,
                ..
            } => {
                token("flow_id", flow_id)?;
                super::validate_flow_version(*flow_version).map_err(str::to_string)?;
                token("flow_install_id", flow_install_id)?;
                hex("flow_code_hash", flow_code_hash, 64)
            }
        }
    }
}

fn valid_name(name: String) -> Option<String> {
    crate::manifest::validate_plain_text(&name, 80)
        .ok()
        .map(|()| name)
}

fn deserialize_name<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_str().and_then(|name| valid_name(name.to_owned())))
}

/// Facts from the local pairing roster. Fixed for the scope's epoch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct FederatedAttributes {
    pub peer_class: FederatedPeerClass,
    pub peer_fingerprint: String,
    pub pairing_id: String,
    pub peer_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_claim: Option<RemoteClaim>,
}

impl FederatedAttributes {
    /// Compare the facts fixed for an epoch, ignoring only the remote claim's
    /// display names. Every other field counts, including any added later;
    /// `peer_name` is a fixed pairing-roster fact, not a display name here.
    pub fn same_identity(&self, other: &Self) -> bool {
        let without_names = |facts: &Self| {
            let mut facts = facts.clone();
            facts.remote_claim = facts.remote_claim.as_ref().map(RemoteClaim::without_names);
            facts
        };
        without_names(self) == without_names(other)
    }

    pub fn new(
        peer_class: FederatedPeerClass,
        peer_fingerprint: String,
        pairing_id: String,
        peer_name: String,
    ) -> Self {
        Self {
            peer_class,
            peer_fingerprint,
            pairing_id,
            peer_name,
            remote_claim: None,
        }
    }

    #[must_use]
    pub fn with_remote_claim(mut self, claim: Option<RemoteClaim>) -> Self {
        self.remote_claim = claim;
        self
    }

    /// Validate both decoded and programmatically constructed attributes.
    pub fn validate(&self) -> Result<(), String> {
        hex("peer_fingerprint", &self.peer_fingerprint, 32)?;
        crate::tool_call::validate_opaque_field("pairing_id", &self.pairing_id)
            .map_err(|e| e.to_string())?;
        crate::manifest::validate_plain_text(&self.peer_name, 80)
            .map_err(|e| format!("peer_name {e}"))?;
        if let Some(claim) = &self.remote_claim {
            claim.validate()?;
        }
        Ok(())
    }
}

fn hex(name: &str, value: &str, length: usize) -> Result<(), String> {
    if value.len() != length
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(format!(
            "{name} must contain exactly {length} lowercase hex characters"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn attributes() -> FederatedAttributes {
        FederatedAttributes::new(
            FederatedPeerClass::Computer,
            "a".repeat(32),
            "pairing".into(),
            "Peer".into(),
        )
    }

    #[test]
    fn federated_claim_names_are_optional_lossy_display_only() {
        for (claim, field) in [
            (RemoteClaim::agent("agent"), "agent_name"),
            (RemoteClaim::run("agent"), "agent_name"),
            (
                RemoteClaim::worker("mcp:module", "a".repeat(64), Some("agent".into())),
                "agent_name",
            ),
            (
                RemoteClaim::flow("flow", 1, "install", "a".repeat(64)),
                "flow_name",
            ),
        ] {
            let original = serde_json::to_value(&claim).unwrap();
            assert!(original.get(field).is_none());
            let built = if field == "flow_name" {
                claim.clone().with_flow_name("Name")
            } else {
                claim.clone().with_agent_name("Name")
            };
            assert_eq!(serde_json::to_value(built).unwrap()[field], "Name");
            let invalid_built = if field == "flow_name" {
                claim.clone().with_flow_name("\n")
            } else {
                claim.clone().with_agent_name("\n")
            };
            assert_eq!(serde_json::to_value(invalid_built).unwrap(), original);
            for name in ["Agent".to_string(), "é".repeat(80), "🦀".repeat(80)] {
                let mut wire = original.clone();
                wire[field] = json!(name);
                let decoded: RemoteClaim = serde_json::from_value(wire.clone()).unwrap();
                decoded.validate().unwrap();
                assert_eq!(serde_json::to_value(&decoded).unwrap(), wire);
                assert_ne!(
                    decoded, claim,
                    "structural equality must detect display changes"
                );
                assert!(decoded.same_identity(&claim));
            }
            for invalid in [
                json!(""),
                json!("x".repeat(81)),
                json!("\n"),
                json!("\u{007f}"),
                json!("\u{0085}"),
                json!("\u{061c}"),
                json!("\u{202e}"),
                json!("\u{2069}"),
                json!(null),
                json!(7),
                json!({"name": "Agent"}),
            ] {
                let mut wire = original.clone();
                wire[field] = invalid;
                let decoded: RemoteClaim = serde_json::from_value(wire).unwrap();
                decoded.validate().unwrap();
                assert_eq!(serde_json::to_value(&decoded).unwrap(), original);
                assert_eq!(decoded, claim);
            }
            for (key, value) in original.as_object().unwrap() {
                if key == "kind" {
                    continue;
                }
                let mut changed = original.clone();
                changed[key] = if let Some(n) = value.as_u64() {
                    json!(n + 1)
                } else {
                    json!(format!("other{}", value.as_str().unwrap()))
                };
                let changed: RemoteClaim = serde_json::from_value(changed).unwrap();
                assert!(
                    !claim.same_identity(&changed),
                    "{key} must remain identity-bearing"
                );
            }
        }
        assert!(!RemoteClaim::agent("agent").same_identity(&RemoteClaim::run("agent")));
    }

    #[test]
    fn federated_claim_roundtrips_and_enforces_every_field_bound() {
        for claim in [
            RemoteClaim::Agent {
                agent_id: "a".repeat(256),
                agent_name: None,
            },
            RemoteClaim::Run {
                agent_id: "a".repeat(256),
                agent_name: None,
            },
            RemoteClaim::Worker {
                owner_module_id: "mcp:module".into(),
                worker_ref: "a".repeat(64),
                on_behalf_of: Some("a".repeat(256)),
                agent_name: None,
            },
            RemoteClaim::Flow {
                flow_id: "a".repeat(256),
                flow_version: 9_007_199_254_740_991,
                flow_install_id: "b".repeat(256),
                flow_code_hash: "f".repeat(64),
                flow_name: None,
            },
        ] {
            claim.validate().unwrap();
            let value = serde_json::to_value(&claim).unwrap();
            assert_eq!(
                serde_json::from_value::<RemoteClaim>(value.clone()).unwrap(),
                claim
            );
            let mut unknown = value.clone();
            unknown["unexpected"] = json!(true);
            assert!(serde_json::from_value::<RemoteClaim>(unknown).is_err());
            for key in ["agent_id", "flow_id", "flow_install_id", "on_behalf_of"] {
                if value.get(key).is_some() {
                    for bad in [
                        "".to_string(),
                        "x".repeat(257),
                        "with space".into(),
                        "\n".into(),
                        "é".into(),
                    ] {
                        let mut invalid = value.clone();
                        invalid[key] = json!(bad);
                        assert!(
                            serde_json::from_value::<RemoteClaim>(invalid)
                                .unwrap()
                                .validate()
                                .is_err(),
                            "{key}"
                        );
                    }
                }
            }
            for key in ["worker_ref", "flow_code_hash"] {
                if value.get(key).is_some() {
                    for bad in [
                        "a".repeat(63),
                        "A".repeat(64),
                        "g".repeat(64),
                        "a".repeat(65),
                    ] {
                        let mut invalid = value.clone();
                        invalid[key] = json!(bad);
                        assert!(serde_json::from_value::<RemoteClaim>(invalid)
                            .unwrap()
                            .validate()
                            .is_err());
                    }
                }
            }
            if value.get("flow_version").is_some() {
                for bad in [0u64, 9_007_199_254_740_992] {
                    let mut invalid = value.clone();
                    invalid["flow_version"] = json!(bad);
                    assert!(serde_json::from_value::<RemoteClaim>(invalid)
                        .unwrap()
                        .validate()
                        .is_err());
                }
            }
            if value.get("owner_module_id").is_some() {
                for bad in ["", "..", "a/b", "a\\b", "\n"] {
                    let mut invalid = value.clone();
                    invalid["owner_module_id"] = json!(bad);
                    assert!(serde_json::from_value::<RemoteClaim>(invalid)
                        .unwrap()
                        .validate()
                        .is_err());
                }
            }
        }
        assert!(serde_json::from_value::<RemoteClaim>(json!({"kind":"future"})).is_err());
    }

    #[test]
    fn federated_attributes_roundtrip_classes_and_enforce_limits_and_unknown_members() {
        for class in [
            FederatedPeerClass::Computer,
            FederatedPeerClass::Phone,
            FederatedPeerClass::Sim,
            FederatedPeerClass::Unknown,
        ] {
            let mut facts = attributes();
            facts.peer_class = class;
            facts.peer_name = "界".repeat(80);
            facts.validate().unwrap();
            let value = serde_json::to_value(&facts).unwrap();
            assert_eq!(
                serde_json::from_value::<FederatedAttributes>(value.clone()).unwrap(),
                facts
            );
            let mut unknown = value;
            unknown["unexpected"] = json!(true);
            assert!(serde_json::from_value::<FederatedAttributes>(unknown).is_err());
        }
        for bad in [
            "",
            "a\n",
            "a\u{061c}",
            "a\u{200e}",
            "a\u{202e}",
            "a\u{2069}",
            &"a".repeat(81),
        ] {
            let mut facts = attributes();
            facts.peer_name = bad.into();
            assert!(facts.validate().is_err());
        }
        for bad in [
            "A".repeat(32),
            "a".repeat(31),
            "a".repeat(33),
            "g".repeat(32),
        ] {
            let mut facts = attributes();
            facts.peer_fingerprint = bad;
            assert!(facts.validate().is_err());
        }
        for bad in ["".to_string(), "x".repeat(257), "space here".into()] {
            let mut facts = attributes();
            facts.pairing_id = bad;
            assert!(facts.validate().is_err());
        }
        let mut facts = attributes();
        facts.pairing_id = "x".repeat(256);
        facts.validate().unwrap();
    }

    #[test]
    fn federated_absent_keeps_nonfederated_stamps_readable_by_the_strict_034_attributes_decoder() {
        // Freeze the 0.34 nested schema rather than decoding with today's types.
        #[derive(Default, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        #[allow(dead_code)]
        struct Attributes034 {
            agent_id: Option<String>,
            delegates: bool,
            on_behalf_of_agent: Option<String>,
            flow_id: Option<String>,
            flow_version: Option<u64>,
            flow_code_hash: Option<String>,
            flow_install_id: Option<String>,
            run_id: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Kind034 {
            Head,
            Worker,
            Ephemeral,
        }
        #[derive(Deserialize)]
        struct Stamp034 {
            kind: Kind034,
            #[serde(default)]
            attributes: Attributes034,
        }
        for (kind, wire_kind) in [
            (crate::scope::ScopeKind::Head, "head"),
            (crate::scope::ScopeKind::Worker, "worker"),
            (crate::scope::ScopeKind::Ephemeral, "ephemeral"),
        ] {
            let stamp = crate::scope::ScopeStamp::new(crate::Principal::Direct, "legacy", 1, kind)
                .with_attributes(
                    crate::scope::ScopeAttributes::new()
                        .with_agent_id(Some("agent".into()))
                        .with_delegates(true),
                );
            let value = serde_json::to_value(stamp).unwrap();
            assert!(value["attributes"].get("federated").is_none());
            assert_eq!(value["kind"], wire_kind);
            let old: Stamp034 = serde_json::from_value(value.clone()).unwrap();
            assert!(matches!(
                old.kind,
                Kind034::Head | Kind034::Worker | Kind034::Ephemeral
            ));
            assert_eq!(old.attributes.agent_id.as_deref(), Some("agent"));
            assert!(old.attributes.delegates);
            let mut bad = value["attributes"].clone();
            bad["federated"] = serde_json::to_value(attributes()).unwrap();
            assert!(serde_json::from_value::<Attributes034>(bad).is_err());
        }
    }
}
