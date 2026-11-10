//! Opt-in config-gate client wire. Plans never commit until `config.apply` is
//! approved; a terminal's claim to have displayed a diff grants no authority.

use serde::{Deserialize, Serialize};
pub use subc_config::{ApprovalChannel, Digest, Generation, Plan};

pub const CONFIG_GATE_CAPABILITY: &str = "config.gate";

/// Standalone bodies keep additions out of the legacy exhaustive control enum.
/// Unknown request fields are refused, including caller-authored review claims.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", deny_unknown_fields)]
#[non_exhaustive]
pub enum ConfigControlRequest {
    #[serde(rename = "config.status")]
    Status {},
    #[serde(rename = "config.plan")]
    Plan {},
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
    },
}

impl ConfigControlRequest {
    pub fn op(&self) -> &'static str {
        match self {
            Self::Status {} => "config.status",
            Self::Plan {} => "config.plan",
            Self::Apply { .. } => "config.apply",
            Self::Stamp {} => "config.stamp",
            Self::Unstamp {} => "config.unstamp",
            Self::History {} => "config.history",
            Self::Rollback { .. } => "config.rollback",
            Self::PlaceProgram { .. } => "config.place_program",
        }
    }
}

/// The trusted surface renders the entire plan, not just the digest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ConfigPlanView {
    pub digest: Digest,
    pub base_revision: u64,
    pub plan: Plan,
}

impl ConfigPlanView {
    pub fn new(plan: Plan) -> Result<Self, subc_config::CanonicalError> {
        Ok(Self {
            digest: plan.digest()?,
            base_revision: plan.base_revision,
            plan,
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
    #[serde(rename = "config.plan")]
    Plan { view: ConfigPlanView },
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
            json!({"op":"config.plan"}),
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
}
