//! Opt-in config-gate client wire. Plans never commit until `config.apply` is
//! approved; a terminal's claim to have displayed a diff grants no authority.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
pub use subc_config::{ApprovalChannel, Digest, Generation, Plan};

pub const CONFIG_GATE_CAPABILITY: &str = "config.gate";
pub const CONFIG_SETTINGS_CAPABILITY: &str = "config.settings/v1";

/// One approved-layer patch. Workspace and project edits currently produce
/// `layer_not_served`; daemon settings cannot be edited through this operation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ConfigEdit {
    pub module_id: String,
    pub layer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub path: String,
    #[serde(flatten)]
    pub action: ConfigEditAction,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConfigEditAction {
    Set { set: serde_json::Value },
    Remove { remove: bool },
}

impl<'de> Deserialize<'de> for ConfigEdit {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            module_id: String,
            layer: String,
            workspace_id: Option<String>,
            project_id: Option<String>,
            path: String,
            #[serde(default, deserialize_with = "present_value")]
            set: Option<serde_json::Value>,
            remove: Option<bool>,
        }
        fn present_value<'de, D: serde::Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<serde_json::Value>, D::Error> {
            serde_json::Value::deserialize(deserializer).map(Some)
        }
        let wire = Wire::deserialize(deserializer)?;
        let action = match (wire.set, wire.remove) {
            (Some(set), None) => ConfigEditAction::Set { set },
            (None, Some(true)) => ConfigEditAction::Remove { remove: true },
            _ => return Err(serde::de::Error::custom("expected set or remove: true")),
        };
        Ok(Self {
            module_id: wire.module_id,
            layer: wire.layer,
            workspace_id: wire.workspace_id,
            project_id: wire.project_id,
            path: wire.path,
            action,
        })
    }
}

/// A field refusal is displayed verbatim and prevents applying the whole plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConfigPlanRefusal {
    pub module_id: String,
    pub path: String,
    pub code: String,
    pub message: String,
}

/// Standalone bodies keep additions out of the legacy exhaustive control enum.
/// Unknown request fields are refused, including caller-authored review claims.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", deny_unknown_fields)]
#[non_exhaustive]
pub enum ConfigControlRequest {
    #[serde(rename = "config.status")]
    Status {},
    #[serde(rename = "config.modules")]
    Modules {},
    #[serde(rename = "config.schema")]
    Schema { module_id: String },
    #[serde(rename = "config.show")]
    Show {
        module_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        through: Option<ConfigThrough>,
    },
    #[serde(rename = "config.watch")]
    Watch {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        modules: Option<Vec<String>>,
    },
    #[serde(rename = "config.plan")]
    Plan {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_generations: Option<BTreeMap<String, u64>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        edits: Option<Vec<ConfigEdit>>,
    },
    #[serde(rename = "config.plan_get")]
    PlanGet { digest: Digest },
    #[serde(rename = "config.plan_discard")]
    PlanDiscard { digest: Digest },
    #[serde(rename = "config.apply")]
    Apply { digest: Digest },
    #[serde(rename = "config.stamp")]
    Stamp {},
    #[serde(rename = "config.unstamp")]
    Unstamp {},
    #[serde(rename = "config.history")]
    History {},
    #[serde(rename = "config.rollback")]
    Rollback { generation: u64 },
    #[serde(rename = "config.place_program")]
    PlaceProgram {
        module_id: String,
        path: String,
        sha256: Digest,
        /// Offline manifest reported by the placement tool, not executed by the daemon.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        manifest: Option<serde_json::Value>,
    },
}

impl ConfigControlRequest {
    pub fn op(&self) -> &'static str {
        match self {
            Self::Status {} => "config.status",
            Self::Modules {} => "config.modules",
            Self::Schema { .. } => "config.schema",
            Self::Show { .. } => "config.show",
            Self::Watch { .. } => "config.watch",
            Self::Plan { .. } => "config.plan",
            Self::PlanGet { .. } => "config.plan_get",
            Self::PlanDiscard { .. } => "config.plan_discard",
            Self::Apply { .. } => "config.apply",
            Self::Stamp {} => "config.stamp",
            Self::Unstamp {} => "config.unstamp",
            Self::History {} => "config.history",
            Self::Rollback { .. } => "config.rollback",
            Self::PlaceProgram { .. } => "config.place_program",
        }
    }
}

/// An inclusive upper bound on the layers merged by `config.show`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigThrough {
    Default,
    User,
    Workspace,
}

/// Status strings are open: clients display an unfamiliar status as raw text.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ConfigModule {
    pub module_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub gated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_digest: Option<Digest>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_plan: Option<Digest>,
}

/// Parsed layer documents, never file paths, raw bytes, or proposal values.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ConfigSettingsLayer {
    pub layer: subc_config::Layer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub document: serde_json::Value,
}

impl ConfigSettingsLayer {
    pub fn new(layer: subc_config::Layer, document: serde_json::Value) -> Self {
        Self {
            layer,
            workspace_id: None,
            project_id: None,
            document,
        }
    }
}

/// A watch always begins with a full resync, including after reconnecting.
/// A resync may arrive at any time and replaces the reader's entire state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ConfigWatchEvent {
    Resync {
        modules: Vec<ConfigModule>,
        stamped: bool,
    },
    Change {
        module_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        approved_generation: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_generation: Option<u64>,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status_reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema_digest: Option<Digest>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pending_plan: Option<Digest>,
    },
}

impl ConfigWatchEvent {
    pub fn change(row: &ConfigModule) -> Self {
        Self::Change {
            module_id: row.module_id.clone(),
            approved_generation: row.approved_generation,
            active_generation: row.active_generation,
            status: row.status.clone(),
            status_reason: row.status_reason.clone(),
            schema_digest: row.schema_digest.clone(),
            pending_plan: row.pending_plan.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ConfigWatchResponse {
    op: ConfigWatchOp,
    #[serde(flatten)]
    pub event: ConfigWatchEvent,
}

impl ConfigWatchResponse {
    pub fn new(event: ConfigWatchEvent) -> Self {
        Self {
            op: ConfigWatchOp::Watch,
            event,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
enum ConfigWatchOp {
    #[serde(rename = "config.watch")]
    Watch,
}

/// The trusted surface renders the entire plan, not just the digest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ConfigPlanView {
    pub digest: Digest,
    pub base_revision: u64,
    pub plan: Plan,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refusals: Vec<ConfigPlanRefusal>,
}

impl ConfigPlanView {
    pub fn new(plan: Plan) -> Result<Self, subc_config::CanonicalError> {
        Ok(Self {
            digest: plan.digest()?,
            base_revision: plan.base_revision,
            plan,
            refusals: Vec::new(),
        })
    }
}

/// The durable review channel for a retained or pruned generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ConfigApprovalRecord {
    pub generation: u64,
    pub channel: ApprovalChannel,
}

impl ConfigApprovalRecord {
    pub fn new(generation: u64, channel: ApprovalChannel) -> Self {
        Self {
            generation,
            channel,
        }
    }
}

/// Diagnostic strings are extensible; they never grant approval authority.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ConfigStatus {
    pub approval_channels: Vec<ConfigApprovalRecord>,
    pub state: String,
    pub integrity: String,
    pub revision: u64,
    pub base_revision: u64,
    pub pending_proposals: u64,
    pub anchor_pending: bool,
    pub ungated_modules: Vec<String>,
    pub ungated_inputs: Vec<String>,
    pub gated_modules: Vec<String>,
    pub interpreter_only_covered: Vec<String>,
    pub approved_but_inactive: Vec<serde_json::Value>,
    pub development_switches: Vec<String>,
    pub missing_approval_surfaces: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op")]
#[non_exhaustive]
pub enum ConfigControlResponse {
    #[serde(rename = "config.status")]
    Status { status: ConfigStatus },
    #[serde(rename = "config.modules")]
    Modules {
        modules: Vec<ConfigModule>,
        stamped: bool,
    },
    #[serde(rename = "config.schema")]
    Schema {
        module_id: String,
        schema_digest: Digest,
        schema: serde_json::Value,
    },
    #[serde(rename = "config.show")]
    Show {
        module_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        through: Option<ConfigThrough>,
        stamped: bool,
        base_generation: u64,
        effective: serde_json::Value,
        sources: std::collections::BTreeMap<String, subc_config::Layer>,
        layers: Vec<ConfigSettingsLayer>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pending_plan: Option<Digest>,
    },
    #[serde(rename = "config.plan")]
    Plan { view: ConfigPlanView },
    #[serde(rename = "config.plan_get")]
    PlanGet { view: ConfigPlanView },
    #[serde(rename = "config.plan_discard")]
    PlanDiscard {},
    #[serde(rename = "config.stamp")]
    Stamp { view: ConfigPlanView },
    #[serde(rename = "config.unstamp")]
    Unstamp { view: ConfigPlanView },
    #[serde(rename = "config.rollback")]
    Rollback { view: ConfigPlanView },
    #[serde(rename = "config.apply")]
    Applied {
        generation: u64,
        #[serde(default)]
        restart_required: bool,
    },
    #[serde(rename = "config.history")]
    History {
        generations: Vec<Generation>,
        pruned: Vec<serde_json::Value>,
    },
    #[serde(rename = "config.place_program")]
    ProgramPlaced {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        generation: Option<u64>,
    },
}

#[cfg(test)]
mod tests {
    #[test]
    fn config_wire_existing_bodies_remain_byte_identical() {
        use super::*;
        assert_eq!(
            serde_json::to_vec(&ConfigControlRequest::Plan {
                base_generations: None,
                edits: None,
            })
            .unwrap(),
            br#"{"op":"config.plan"}"#
        );
        assert_eq!(
            serde_json::to_vec(&ConfigControlRequest::Status {}).unwrap(),
            br#"{"op":"config.status"}"#
        );
        assert_eq!(
            serde_json::to_vec(&ConfigControlRequest::History {}).unwrap(),
            br#"{"op":"config.history"}"#
        );
        assert_eq!(
            serde_json::to_vec(&ConfigControlResponse::History {
                generations: vec![],
                pruned: vec![]
            })
            .unwrap(),
            br#"{"op":"config.history","generations":[],"pruned":[]}"#
        );
        assert_eq!(serde_json::to_vec(&ConfigControlResponse::Status { status: ConfigStatus::default() }).unwrap(), br#"{"op":"config.status","status":{"approval_channels":[],"state":"","integrity":"","revision":0,"base_revision":0,"pending_proposals":0,"anchor_pending":false,"ungated_modules":[],"ungated_inputs":[],"gated_modules":[],"interpreter_only_covered":[],"approved_but_inactive":[],"development_switches":[],"missing_approval_surfaces":[],"warnings":[]}}"#);
    }

    #[test]
    fn config_wire_settings_status_is_open_and_selectors_are_strict() {
        use super::*;
        let module: ConfigModule = serde_json::from_value(
            serde_json::json!({"module_id":"sample","gated":true,"status":"future_status"}),
        )
        .unwrap();
        assert_eq!(module.status, "future_status");
        assert!(serde_json::from_value::<ConfigControlRequest>(
            serde_json::json!({"op":"config.show","module_id":"sample","through":"project"})
        )
        .is_err());
        let event = ConfigWatchResponse::new(ConfigWatchEvent::Resync {
            modules: vec![module],
            stamped: false,
        });
        assert_eq!(
            serde_json::from_slice::<ConfigWatchResponse>(&serde_json::to_vec(&event).unwrap())
                .unwrap(),
            event
        );
    }

    #[test]
    fn config_wire_program_placement_names_and_optional_generation() {
        use super::*;
        let body = serde_json::json!({"op":"config.place_program","module_id":"demo","path":"/bin/demo","sha256":format!("sha256:{}", "a".repeat(64))});
        let request: ConfigControlRequest = serde_json::from_value(body.clone()).unwrap();
        assert_eq!(request.op(), "config.place_program");
        assert_eq!(serde_json::to_value(request).unwrap(), body);
        assert!(matches!(
            serde_json::from_value::<ConfigControlResponse>(
                serde_json::json!({"op":"config.place_program"})
            )
            .unwrap(),
            ConfigControlResponse::ProgramPlaced { generation: None }
        ));
        let response = ConfigControlResponse::ProgramPlaced {
            generation: Some(3),
        };
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            serde_json::json!({"op":"config.place_program","generation":3})
        );
    }
    #[test]
    fn config_wire_applied_reports_restart_required_and_defaults_absent_to_false() {
        use super::*;
        let response = ConfigControlResponse::Applied {
            generation: 7,
            restart_required: true,
        };
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            serde_json::json!({"op":"config.apply", "generation":7, "restart_required":true})
        );
        assert_eq!(
            serde_json::from_value::<ConfigControlResponse>(
                serde_json::json!({"op":"config.apply", "generation":7})
            )
            .unwrap(),
            ConfigControlResponse::Applied {
                generation: 7,
                restart_required: false,
            }
        );
        assert_eq!(
            serde_json::to_value(ConfigControlResponse::Applied {
                generation: 7,
                restart_required: false,
            })
            .unwrap()["restart_required"],
            false
        );
    }

    #[test]
    fn config_wire_status_and_history_preserve_unverified_approval_channel() {
        use super::*;
        let channel = ApprovalChannel::UnverifiedDevelopmentDesktop;
        let mut status = ConfigStatus::default();
        status
            .approval_channels
            .push(ConfigApprovalRecord::new(1, channel));
        let generation = Generation::new(
            1,
            channel,
            subc_config::raw_digest(b"plan"),
            subc_config::InputVector::default(),
            subc_config::Changeset::new(),
        );
        for response in [
            ConfigControlResponse::Status { status },
            ConfigControlResponse::History {
                generations: vec![generation],
                pruned: vec![],
            },
        ] {
            let encoded = serde_json::to_string(&response).unwrap();
            assert!(encoded.contains("unverified_development_desktop"));
            assert_eq!(
                serde_json::from_str::<ConfigControlResponse>(&encoded).unwrap(),
                response
            );
        }
    }
    use super::*;
    use serde_json::json;

    #[test]
    fn config_wire_requests_are_strict_and_round_trip() {
        for body in [
            json!({"op":"config.status"}),
            json!({"op":"config.modules"}),
            json!({"op":"config.schema","module_id":"sample-reader"}),
            json!({"op":"config.show","module_id":"sample-reader"}),
            json!({"op":"config.show","module_id":"sample-reader","project_id":"sample-project","through":"workspace"}),
            json!({"op":"config.watch"}),
            json!({"op":"config.watch","modules":["sample-reader"]}),
            json!({"op":"config.plan"}),
            json!({"op":"config.plan_get", "digest":subc_config::raw_digest(b"plan")}),
            json!({"op":"config.plan_discard", "digest":subc_config::raw_digest(b"plan")}),
            json!({"op":"config.apply", "digest":subc_config::raw_digest(b"plan")}),
            json!({"op":"config.stamp"}),
            json!({"op":"config.unstamp"}),
            json!({"op":"config.history"}),
            json!({"op":"config.rollback", "generation":4}),
        ] {
            let request: ConfigControlRequest = serde_json::from_value(body.clone()).unwrap();
            assert_eq!(body["op"], request.op());
            assert_eq!(serde_json::to_value(&request).unwrap(), body);
            let mut claimed = body;
            claimed["reviewed"] = json!(true);
            assert!(serde_json::from_value::<ConfigControlRequest>(claimed).is_err());
        }
        assert!(
            serde_json::from_value::<ConfigControlRequest>(json!({"op":"config.apply"})).is_err()
        );
    }

    #[test]
    fn config_wire_plan_view_and_status_tolerate_reply_extensions() {
        let plan = Plan::new(7, 3, "daemon", Default::default(), Default::default());
        let view = ConfigPlanView::new(plan.clone()).unwrap();
        assert_eq!(view.digest, plan.digest().unwrap());
        let response = ConfigControlResponse::Stamp { view };
        let mut body = serde_json::to_value(&response).unwrap();
        body["future"] = json!(true);
        assert_eq!(
            serde_json::from_value::<ConfigControlResponse>(body).unwrap(),
            response
        );
        let status = ConfigControlResponse::Status {
            status: ConfigStatus::default(),
        };
        assert_eq!(
            serde_json::from_value::<ConfigControlResponse>(serde_json::to_value(&status).unwrap())
                .unwrap(),
            status
        );
    }

    #[test]
    fn config_wire_edit_actions_are_exclusive_and_null_is_not_absence() {
        let base = json!({"module_id":"demo","layer":"user","path":"/limit"});
        for action in [
            json!({"set":0}),
            json!({"set":false}),
            json!({"set":null}),
            json!({"remove":true}),
        ] {
            let mut value = base.clone();
            value
                .as_object_mut()
                .unwrap()
                .extend(action.as_object().unwrap().clone());
            let edit: ConfigEdit = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(edit).unwrap(), value);
        }
        for action in [
            json!({}),
            json!({"set":1,"remove":true}),
            json!({"remove":false}),
            json!({"remove":true,"extra":1}),
        ] {
            let mut value = base.clone();
            value
                .as_object_mut()
                .unwrap()
                .extend(action.as_object().unwrap().clone());
            assert!(serde_json::from_value::<ConfigEdit>(value).is_err());
        }
        let request: ConfigControlRequest =
            serde_json::from_value(json!({"op":"config.plan"})).unwrap();
        assert_eq!(
            serde_json::to_vec(&request).unwrap(),
            br#"{"op":"config.plan"}"#
        );
    }

    #[test]
    fn config_wire_empty_refusals_preserve_legacy_view_bytes() {
        let plan = Plan::new(7, 3, "daemon", Default::default(), Default::default());
        let view = ConfigPlanView::new(plan.clone()).unwrap();
        let legacy = json!({"digest":plan.digest().unwrap(),"base_revision":7,"plan":plan});
        assert_eq!(serde_json::to_value(&view).unwrap(), legacy);
        assert_eq!(
            serde_json::from_value::<ConfigPlanView>(legacy).unwrap(),
            view
        );
        assert_eq!(
            serde_json::to_string(&ConfigControlResponse::PlanDiscard {}).unwrap(),
            r#"{"op":"config.plan_discard"}"#
        );
    }
}
