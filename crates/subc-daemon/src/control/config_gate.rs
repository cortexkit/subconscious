use subc_control::config::ConfigControlRequest;

use super::*;
use crate::config_gate::{ApprovalChallenge, PresenceDecision};

pub(super) fn is_config_op(op: &str) -> bool {
    matches!(
        op,
        "config.status"
            | "config.plan"
            | "config.apply"
            | "config.stamp"
            | "config.unstamp"
            | "config.history"
            | "config.rollback"
    )
}

impl PrivilegedRequest for ConfigControlRequest {
    fn privileged_operation(&self) -> Option<PrivilegedOperation<'_>> {
        // The config gate binds trusted review and presence to its digest. The
        // generic operator prompt would run twice, and its log/shortcut modes
        // cannot substitute for config approval.
        match self {
            Self::Status {} => None,
            Self::Plan {} => None,
            Self::Apply { .. } => None,
            Self::Stamp {} => None,
            Self::Unstamp {} => None,
            Self::History {} => None,
            Self::Rollback { .. } => None,
            _ => None,
        }
    }
}

impl ControlHandler {
    pub(super) async fn handle_config_gate_request(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        let request = match serde_json::from_slice::<ConfigControlRequest>(&frame.body) {
            Ok(request) => request,
            Err(error) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_control_body",
                    format!("malformed config control body: {error}"),
                )?])
            }
        };
        debug_assert!(privileged_operation(&request).is_none());
        // No config capability is advertised until the runtime owner connects
        // snapshots, authority-store commits and supervisor delivery together.
        let _ = ctx;
        Ok(vec![control_error_frame(
            &frame,
            "config_gate_inactive",
            "configuration gate runtime is not connected",
        )?])
    }

    /// Execute only the presence part of an engine-selected challenge. The
    /// runtime owner still rechecks the inputs and surface before committing.
    pub async fn confirm_config_gate_presence(
        &self,
        ctx: &RouteCtx,
        frame: &Frame,
        challenge: &ApprovalChallenge,
    ) -> PresenceDecision {
        let Some(text) = challenge.prompt() else {
            return PresenceDecision::Unavailable;
        };
        let caller = self.resolve_caller(ctx.connection_id).await;
        let key = crate::operator_confirm::DeclineKey::for_caller(&caller, ctx.connection_id);
        // Neither privileged_operator_prompt nor operator-app shortcut policy
        // is consulted: every desktop gate review requires OS presence.
        let outcome = self
            .forwarding
            .operator_confirms()
            .approve_config_gate(ctx, frame, text.to_owned(), caller.description(), key)
            .await;
        PresenceDecision::from_operator_outcome(&outcome)
    }
}

#[cfg(test)]
mod tests;
