use serde::{Deserialize, Serialize};

use super::RootKey;
use crate::Principal;

/// The paired peer's class, attested by the configured federated owner.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FederatedPeerClass {
    Computer,
    App,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
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
        /// The flow's owning agent from the sending flow scope; absent for
        /// operator-installed global flows.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
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
            run_id: None,
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
            agent_id: None,
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

    /// Set the run id. Has no effect on agent, worker or flow claims.
    #[must_use]
    pub fn with_run_id(mut self, run_id: impl Into<String>) -> Self {
        if let Self::Run { run_id: value, .. } = &mut self {
            *value = Some(run_id.into());
        }
        self
    }

    /// Set the flow's owning agent. Has no effect on agent, run or worker claims.
    #[must_use]
    pub fn with_flow_agent(mut self, agent_id: impl Into<String>) -> Self {
        if let Self::Flow {
            agent_id: value, ..
        } = &mut self
        {
            *value = Some(agent_id.into());
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
            Self::Agent { agent_id, .. } => token("agent_id", agent_id),
            Self::Run {
                agent_id, run_id, ..
            } => {
                token("agent_id", agent_id)?;
                if let Some(run_id) = run_id {
                    super::validate_run_id(run_id).map_err(|e| e.to_string())?;
                }
                Ok(())
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
                agent_id,
                ..
            } => {
                token("flow_id", flow_id)?;
                super::validate_flow_version(*flow_version).map_err(str::to_string)?;
                token("flow_install_id", flow_install_id)?;
                if let Some(agent_id) = agent_id {
                    token("agent_id", agent_id)?;
                }
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

/// Portable project key and repository root key from the sending scope stamp,
/// carried by the pairing module. Together they name a folder portably:
/// receivers resolve the pair through their own project registry and never use
/// a path from another computer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct FederatedProject {
    pub project_id: String,
    pub root_key: RootKey,
}

impl FederatedProject {
    pub fn new(project_id: impl Into<String>, root_key: RootKey) -> Self {
        Self {
            project_id: project_id.into(),
            root_key,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        crate::tool_call::validate_opaque_field("project_id", &self.project_id)
            .map_err(|e| e.to_string())?;
        self.root_key.validate().map_err(str::to_string)
    }
}

/// Facts from the local pairing roster. Fixed for the scope's epoch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct FederatedAttributes {
    /// Require a federated stamp instead of falling back to a legacy bind.
    /// This daemon instruction is removed from provider-facing stamps.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stamp_required: bool,
    pub peer_class: FederatedPeerClass,
    pub peer_fingerprint: String,
    /// Stable paired computer machine id from the pairing roster, absent for
    /// phones and simulators. Unlike `peer_fingerprint`, it survives a re-key,
    /// so receivers comparing records of which computer hosts a resource use
    /// this id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_box_id: Option<String>,
    pub pairing_id: String,
    pub peer_name: String,
    /// The principal the sending computer's daemon stamped on the route that
    /// opened this call, carried by the pairing module. It sits beside the claim
    /// because a module-to-module call carries no claim. Only `reserved` names
    /// a launch-verified module.
    #[serde(
        default,
        deserialize_with = "deserialize_origin_principal",
        skip_serializing_if = "Option::is_none"
    )]
    pub origin_principal: Option<Principal>,
    /// Portable project and root keys from the sending session's scope stamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<FederatedProject>,
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
            stamp_required: false,
            peer_fingerprint,
            peer_box_id: None,
            pairing_id,
            peer_name,
            origin_principal: None,
            project: None,
            remote_claim: None,
        }
    }

    #[must_use]
    pub fn with_remote_claim(mut self, claim: Option<RemoteClaim>) -> Self {
        self.remote_claim = claim;
        self
    }

    #[must_use]
    pub fn with_origin_principal(mut self, principal: Option<Principal>) -> Self {
        self.origin_principal = principal;
        self
    }

    #[must_use]
    pub fn with_project(mut self, project: Option<FederatedProject>) -> Self {
        self.project = project;
        self
    }

    /// Validate both decoded and programmatically constructed attributes.
    pub fn validate(&self) -> Result<(), String> {
        hex("peer_fingerprint", &self.peer_fingerprint, 32)?;
        if let Some(peer_box_id) = &self.peer_box_id {
            hex("peer_box_id", peer_box_id, 32)?;
        }
        match &self.origin_principal {
            Some(Principal::Reserved { module_id }) => {
                crate::manifest::validate_module_id(module_id).map_err(|e| e.to_string())?;
            }
            Some(Principal::Unverified) => return Err("origin_principal is unverified".into()),
            Some(Principal::Direct) | None => {}
        }
        if let Some(project) = &self.project {
            project.validate()?;
        }
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

fn deserialize_origin_principal<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Principal>, D::Error> {
    Option::<super::ScopePrincipal>::deserialize(deserializer)
        .map(|principal| principal.map(Into::into))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::RootKeyKind;
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
    fn federated_stamp_required_defaults_false_and_is_omitted_unless_required() {
        let mut facts = attributes();
        let value = serde_json::to_value(&facts).unwrap();
        assert!(value.get("stamp_required").is_none());
        assert!(
            !serde_json::from_value::<FederatedAttributes>(value)
                .unwrap()
                .stamp_required
        );
        facts.stamp_required = true;
        let value = serde_json::to_value(&facts).unwrap();
        assert_eq!(value["stamp_required"], true);
        assert!(
            serde_json::from_value::<FederatedAttributes>(value)
                .unwrap()
                .stamp_required
        );
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
                run_id: None,
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
                agent_id: None,
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
            FederatedPeerClass::App,
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
    fn federated_new_members_roundtrip_and_old_senders_decode_unchanged() {
        let old = json!({
            "peer_class": "computer", "peer_fingerprint": "a".repeat(32),
            "pairing_id": "pairing", "peer_name": "Peer"
        });
        let decoded: FederatedAttributes = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(decoded, attributes());
        assert_eq!(serde_json::to_value(decoded).unwrap(), old);
        for claim in [
            RemoteClaim::run("agent"),
            RemoteClaim::flow("flow", 1, "install", "b".repeat(64)),
        ] {
            let value = serde_json::to_value(&claim).unwrap();
            assert!(value.get("run_id").is_none());
            if matches!(claim, RemoteClaim::Flow { .. }) {
                assert!(value.get("agent_id").is_none());
            }
            assert_eq!(serde_json::from_value::<RemoteClaim>(value).unwrap(), claim);
        }
        for principal in [
            Principal::Direct,
            Principal::Reserved {
                module_id: "origin-module".into(),
            },
        ] {
            for claim in [
                RemoteClaim::run("agent").with_run_id("run-1"),
                RemoteClaim::flow("flow", 1, "install", "b".repeat(64)).with_flow_agent("agent"),
            ] {
                let mut facts = attributes()
                    .with_origin_principal(Some(principal.clone()))
                    .with_remote_claim(Some(claim));
                facts.peer_class = FederatedPeerClass::App;
                facts.peer_box_id = Some("0123456789abcdef".repeat(2));
                facts = facts.with_project(Some(FederatedProject::new(
                    "project",
                    RootKey::new(RootKeyKind::Remote, "owner/repo"),
                )));
                facts.validate().unwrap();
                let wire = serde_json::to_value(&facts).unwrap();
                assert_eq!(wire["peer_class"], "app");
                assert_eq!(
                    wire["origin_principal"],
                    serde_json::to_value(&principal).unwrap()
                );
                assert_eq!(wire["peer_box_id"], "0123456789abcdef".repeat(2));
                assert_eq!(
                    wire["project"],
                    json!({"project_id":"project", "root_key":{"kind":"remote", "key":"owner/repo"}})
                );
                assert_eq!(
                    serde_json::from_value::<FederatedAttributes>(wire).unwrap(),
                    facts
                );
            }
        }
        assert_eq!(
            RemoteClaim::agent("agent")
                .with_run_id("run")
                .with_flow_agent("owner"),
            RemoteClaim::agent("agent")
        );
        assert_eq!(
            RemoteClaim::run("agent").with_flow_agent("owner"),
            RemoteClaim::run("agent")
        );
        let flow = RemoteClaim::flow("flow", 1, "install", "b".repeat(64));
        assert_eq!(flow.clone().with_run_id("run"), flow);
        let worker = RemoteClaim::worker("owner", "b".repeat(64), None);
        assert_eq!(
            worker.clone().with_run_id("run").with_flow_agent("owner"),
            worker
        );
    }

    #[test]
    fn federated_new_identity_members_enforce_bounds() {
        for module_id in [
            "".into(),
            ".".into(),
            "..".into(),
            "a/b".into(),
            "a\\b".into(),
            "\n".into(),
            "x".repeat(256),
        ] {
            assert!(attributes()
                .with_origin_principal(Some(Principal::Reserved { module_id }))
                .validate()
                .is_err());
        }
        attributes()
            .with_origin_principal(Some(Principal::Reserved {
                module_id: "x".repeat(255),
            }))
            .validate()
            .unwrap();
        attributes()
            .with_origin_principal(Some(Principal::Direct))
            .validate()
            .unwrap();
        assert!(attributes()
            .with_origin_principal(Some(Principal::Unverified))
            .validate()
            .is_err());
        for value in [
            "".into(),
            "x".repeat(257),
            "has space".into(),
            "\n".into(),
            "é".into(),
            "\u{007f}".into(),
        ] {
            assert!(RemoteClaim::run("agent")
                .with_run_id(&value)
                .validate()
                .is_err());
            assert!(RemoteClaim::flow("flow", 1, "install", "b".repeat(64))
                .with_flow_agent(&value)
                .validate()
                .is_err());
            assert!(attributes()
                .with_project(Some(FederatedProject::new(
                    &value,
                    RootKey::new(RootKeyKind::Remote, "owner/repo")
                )))
                .validate()
                .is_err());
            assert!(attributes()
                .with_project(Some(FederatedProject::new(
                    "project",
                    RootKey::new(RootKeyKind::Label, &value)
                )))
                .validate()
                .is_err());
        }
        for value in ["!".to_owned(), "~".repeat(256)] {
            RemoteClaim::run("agent")
                .with_run_id(&value)
                .validate()
                .unwrap();
            RemoteClaim::flow("flow", 1, "install", "b".repeat(64))
                .with_flow_agent(&value)
                .validate()
                .unwrap();
            attributes()
                .with_project(Some(FederatedProject::new(
                    &value,
                    RootKey::new(RootKeyKind::Label, "root"),
                )))
                .validate()
                .unwrap();
        }
        for value in [
            "".into(),
            "a".repeat(31),
            "a".repeat(33),
            "A".repeat(32),
            "g".repeat(32),
        ] {
            let mut facts = attributes();
            facts.peer_box_id = Some(value);
            assert!(facts.validate().is_err());
        }
        for principal in [
            json!({"kind":"reserved", "module_id":"origin", "extra":true}),
            json!({"kind":"direct", "extra":true}),
            json!({"kind":"future"}),
        ] {
            let mut wire = serde_json::to_value(attributes()).unwrap();
            wire["origin_principal"] = principal;
            assert!(serde_json::from_value::<FederatedAttributes>(wire).is_err());
        }
    }

    #[test]
    fn federated_project_requires_both_keys_and_refuses_unknown_members() {
        for kind in [RootKeyKind::Remote, RootKeyKind::Label] {
            let project = FederatedProject::new("project", RootKey::new(kind, "~".repeat(128)));
            project.validate().unwrap();
            let wire = serde_json::to_value(&project).unwrap();
            assert_eq!(
                serde_json::from_value::<FederatedProject>(wire).unwrap(),
                project
            );
        }
        assert!(FederatedProject::new(
            "project",
            RootKey::new(RootKeyKind::Remote, "x".repeat(129))
        )
        .validate()
        .is_err());
        for project in [
            json!({}),
            json!({"project_id":"project"}),
            json!({"root_key":{"kind":"remote", "key":"owner/repo"}}),
            json!({"project_id":"project", "root_key":"owner/repo"}),
            json!({"project_id":"project", "root_key":{"kind":"future", "key":"owner/repo"}}),
            json!({"project_id":"project", "root_key":{"kind":"remote", "key":"owner/repo", "extra":true}}),
            json!({"project_id":"project", "root_key":{"kind":"remote", "key":"owner/repo"}, "extra":true}),
        ] {
            let mut wire = serde_json::to_value(attributes()).unwrap();
            wire["project"] = project;
            assert!(serde_json::from_value::<FederatedAttributes>(wire).is_err());
        }
    }

    #[test]
    fn federated_same_identity_counts_every_new_member() {
        let base = attributes();
        let mut with_box = base.clone();
        with_box.peer_box_id = Some("b".repeat(32));
        let mut app = base.clone();
        app.peer_class = FederatedPeerClass::App;
        let project = base.clone().with_project(Some(FederatedProject::new(
            "project-one",
            RootKey::new(RootKeyKind::Remote, "owner/repo"),
        )));
        for changed in [
            with_box.clone(),
            project.clone(),
            app,
            base.clone().with_origin_principal(Some(Principal::Direct)),
        ] {
            assert!(!base.same_identity(&changed));
        }
        let mut other_box = with_box.clone();
        other_box.peer_box_id = Some("c".repeat(32));
        assert!(!with_box.same_identity(&other_box));
        for other_project in [
            FederatedProject::new(
                "project-two",
                RootKey::new(RootKeyKind::Remote, "owner/repo"),
            ),
            FederatedProject::new(
                "project-one",
                RootKey::new(RootKeyKind::Remote, "other/repo"),
            ),
            FederatedProject::new(
                "project-one",
                RootKey::new(RootKeyKind::Label, "owner/repo"),
            ),
        ] {
            assert!(!project.same_identity(&base.clone().with_project(Some(other_project))));
        }
        assert!(!base
            .clone()
            .with_origin_principal(Some(Principal::Direct))
            .same_identity(
                &base
                    .clone()
                    .with_origin_principal(Some(Principal::Reserved {
                        module_id: "origin".into()
                    }))
            ));
        for (old, new, other) in [
            (
                RemoteClaim::run("agent"),
                RemoteClaim::run("agent").with_run_id("one"),
                RemoteClaim::run("agent").with_run_id("two"),
            ),
            (
                RemoteClaim::flow("flow", 1, "install", "b".repeat(64)),
                RemoteClaim::flow("flow", 1, "install", "b".repeat(64)).with_flow_agent("one"),
                RemoteClaim::flow("flow", 1, "install", "b".repeat(64)).with_flow_agent("two"),
            ),
        ] {
            assert!(!old.same_identity(&new));
            assert!(!new.same_identity(&other));
            assert!(!base
                .clone()
                .with_remote_claim(Some(old))
                .same_identity(&base.clone().with_remote_claim(Some(new.clone()))));
            assert!(new.same_identity(&new.clone().with_agent_name("Name").with_flow_name("Name")));
        }
    }

    #[test]
    fn federated_readers_first_rollout_new_members_are_refused_by_strict_0373_readers() {
        // Freeze the 0.37.3 shapes: senders must wait until receivers have the
        // new readers, because each addition is refused by the old schema.
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Peer0373 {
            Computer,
            Phone,
            Sim,
            Unknown,
        }
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
        #[allow(dead_code)]
        enum Claim0373 {
            Agent {
                agent_id: String,
                #[serde(default, deserialize_with = "deserialize_name")]
                agent_name: Option<String>,
            },
            Worker {
                owner_module_id: String,
                worker_ref: String,
                #[serde(default)]
                on_behalf_of: Option<String>,
                #[serde(default, deserialize_with = "deserialize_name")]
                agent_name: Option<String>,
            },
            Run {
                agent_id: String,
                #[serde(default, deserialize_with = "deserialize_name")]
                agent_name: Option<String>,
            },
            Flow {
                flow_id: String,
                flow_version: u64,
                flow_install_id: String,
                flow_code_hash: String,
                #[serde(default, deserialize_with = "deserialize_name")]
                flow_name: Option<String>,
            },
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        #[allow(dead_code)]
        struct Attributes0373 {
            #[serde(default)]
            stamp_required: bool,
            peer_class: Peer0373,
            peer_fingerprint: String,
            pairing_id: String,
            peer_name: String,
            #[serde(default)]
            remote_claim: Option<Claim0373>,
        }
        let base = serde_json::to_value(attributes()).unwrap();
        serde_json::from_value::<Attributes0373>(base.clone()).unwrap();
        for (field, value) in [
            ("peer_class", json!("app")),
            ("peer_box_id", json!("b".repeat(32))),
            (
                "project",
                json!({"project_id":"project", "root_key":{"kind":"remote", "key":"owner/repo"}}),
            ),
            ("origin_principal", json!({"kind":"direct"})),
            (
                "remote_claim",
                json!({"kind":"run", "agent_id":"agent", "run_id":"run"}),
            ),
            (
                "remote_claim",
                json!({"kind":"flow", "flow_id":"flow", "flow_version":1, "flow_install_id":"install", "flow_code_hash":"b".repeat(64), "agent_id":"agent"}),
            ),
        ] {
            let mut wire = base.clone();
            wire[field] = value;
            let new: FederatedAttributes = serde_json::from_value(wire.clone()).unwrap();
            new.validate().unwrap();
            assert!(
                serde_json::from_value::<Attributes0373>(wire).is_err(),
                "{field}"
            );
        }
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
