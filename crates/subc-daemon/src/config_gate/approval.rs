use crate::caller_identity::CallerIdentity;
use crate::privileged_prompt::gate_prompt_text;
use subc_config::{Digest, RecordedValue};
use subc_protocol::consent::{ConsentRequest, ConsentResponse};

use super::{ConfigGate, GateError, GateSnapshot, InputKey, PendingPlan, PlanKind, Result};
use crate::config_store::ConfigStore;

/// Observed facts from the active registration, its approved declaration/entry,
/// and the supervisor's current spawn. A claimed module id alone proves none of
/// these. The owner re-reads every fact when a consent reply arrives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhoneSurface {
    pub module_id: String,
    pub connection: u64,
    pub spawn_generation: u64,
    pub enabled_by_policy: bool,
    pub opted_in: bool,
    pub approved_declaration: bool,
    pub reserved: bool,
    pub nonce_verified: bool,
    pub program_admitted: bool,
    pub active_registration: bool,
}
impl PhoneSurface {
    fn trusted(&self) -> bool {
        self.module_id == "callosum"
            && self.enabled_by_policy
            && self.opted_in
            && self.approved_declaration
            && self.reserved
            && self.nonce_verified
            && self.program_admitted
            && self.active_registration
    }
}

/// Built from daemon-observed peer/app state, never deserialized from a request.
/// An unattested connection must still be identified as the desktop app under
/// the development feature; arbitrary owner-uid clients are not desktop apps.
pub struct ApprovalFacts {
    pub connection: u64,
    pub owner_uid: bool,
    pub desktop_app: bool,
    pub attested_operator_app: bool,
    pub os_prompt_available: bool,
    pub phone: Option<PhoneSurface>,
    pub caller: CallerIdentity,
}
impl ApprovalFacts {
    fn desktop(&self) -> bool {
        self.owner_uid
            && (self.attested_operator_app
                || (cfg!(feature = "dev-desktop-approval") && self.desktop_app))
    }
}

#[derive(Clone, Debug)]
pub enum ApprovalChallenge {
    Desktop {
        attempt: u64,
        prompt: String,
    },
    OsPrompt {
        attempt: u64,
        prompt: String,
    },
    Phone {
        attempt: u64,
        connection: u64,
        correlation: u64,
        request: Box<ConsentRequest>,
    },
}
impl ApprovalChallenge {
    pub fn attempt(&self) -> u64 {
        match self {
            Self::Desktop { attempt, .. }
            | Self::OsPrompt { attempt, .. }
            | Self::Phone { attempt, .. } => *attempt,
        }
    }
    pub fn prompt(&self) -> Option<&str> {
        match self {
            Self::Desktop { prompt, .. } | Self::OsPrompt { prompt, .. } => Some(prompt),
            Self::Phone { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresenceDecision {
    InteractiveApproval,
    Declined,
    Unavailable,
}

pub enum ApprovalReply {
    Presence(PresenceDecision),
    Phone {
        connection: u64,
        correlation: u64,
        response: Option<ConsentResponse>,
    },
}

pub(super) enum Surface {
    Desktop,
    Os,
    Phone {
        facts: PhoneSurface,
        request: Box<ConsentRequest>,
    },
}
pub(super) struct Attempt {
    digest: Digest,
    caller: u64,
    surface: Surface,
}

fn unavailable() -> GateError {
    GateError::new(
        "approval_surface_unavailable",
        "required trusted review surface is unavailable",
    )
}
fn declined() -> GateError {
    GateError::new("approval_declined", "approval was declined or unanswered")
}

impl PendingPlan {
    fn desktop_only(&self) -> bool {
        if self.recovery && self.kind == PlanKind::Unstamp {
            return false;
        }
        self.kind == PlanKind::Stamp
            || self.callosum_entry_changed
            || self
                .view
                .plan
                .changeset
                .launch_manifests
                .contains_key("callosum")
            || self
                .changed
                .iter()
                .any(|key| key.module() == Some("callosum"))
            || self
                .view
                .plan
                .changeset
                .fields
                .iter()
                .any(|field| field.module_id == "callosum")
    }

    fn small_prompt(&self, caller: &CallerIdentity) -> Option<String> {
        if self.recovery && self.kind == PlanKind::Unstamp {
            return gate_prompt_text("Config files become authoritative again", &[], caller);
        }
        let changes = &self.view.plan.changeset;
        if self.kind != PlanKind::Change
            || self.desktop_only()
            || changes.fields.len() != 1
            || !changes.policy_changes.is_empty()
            || !changes.launch_manifests.is_empty()
            || !changes.affected_projects.is_empty()
            || !changes.apply_plan["review"].is_null()
            || self.changed.len() != 1
            || self
                .changed
                .iter()
                .any(|key| !matches!(key, InputKey::Layer { .. }))
        {
            return None;
        }
        let field = &changes.fields[0];
        if self
            .changed
            .iter()
            .any(|key| key.module() != Some(field.module_id.as_str()))
        {
            return None;
        }
        if field.project_id.is_some() || !field.reasons.is_empty() {
            return None;
        }
        let scalar = |value: &RecordedValue| match value {
            RecordedValue::Value { value }
                if value.is_boolean() || value.is_number() || value.is_string() =>
            {
                // Inspect the original string too: JSON escaping would otherwise
                // hide a sanitizer substitution behind printable backslashes.
                if let Some(text) = value.as_str() {
                    gate_prompt_text(text, &[], &CallerIdentity::default())?;
                }
                serde_json::to_string(value).ok()
            }
            _ => None,
        };
        let old = scalar(&field.old)?;
        let new = scalar(&field.new)?;
        let effect = serde_json::to_value(field.apply).ok()?;
        let old_source = serde_json::to_value(field.old_source).ok()?;
        let new_source = serde_json::to_value(field.new_source).ok()?;
        let description = format!(
            "Set {} {} from {old} ({}) to {new} ({}); {}",
            field.module_id,
            field.path,
            old_source.as_str()?,
            new_source.as_str()?,
            effect.as_str()?
        );
        gate_prompt_text(&description, &[&field.module_id, &field.path], caller)
    }
}

impl ConfigGate {
    /// Select a required surface. Legacy `log` policy and operator-app shortcuts
    /// are deliberately absent: neither proves review of an immutable plan.
    pub fn begin_approval(
        &mut self,
        digest: &Digest,
        store: &mut ConfigStore,
        current: &GateSnapshot,
        facts: &ApprovalFacts,
    ) -> Result<ApprovalChallenge> {
        let plan = self
            .plans
            .get(digest)
            .ok_or_else(|| GateError::new("plan_unknown", "plan is absent or already applied"))?;
        Self::fresh(plan, store, current)?;
        if !facts.owner_uid {
            return Err(unavailable());
        }
        if self.attempts.len() >= 4 {
            return Err(GateError::new("config_busy", "approval queue is full"));
        }
        self.next_attempt = self.next_attempt.checked_add(1).ok_or_else(unavailable)?;
        let id = self.next_attempt;
        let (surface, challenge) = if facts.desktop() {
            if !facts.os_prompt_available {
                return Err(unavailable());
            }
            let prompt =
                gate_prompt_text(&format!("Approve config plan {digest}"), &[], &facts.caller)
                    .ok_or_else(unavailable)?;
            (
                Surface::Desktop,
                ApprovalChallenge::Desktop {
                    attempt: id,
                    prompt,
                },
            )
        } else if plan.desktop_only() {
            return Err(unavailable());
        } else if let Some(prompt) = plan.small_prompt(&facts.caller) {
            if !facts.os_prompt_available {
                return Err(unavailable());
            }
            (
                Surface::Os,
                ApprovalChallenge::OsPrompt {
                    attempt: id,
                    prompt,
                },
            )
        } else {
            let phone = facts
                .phone
                .as_ref()
                .filter(|phone| !plan.recovery && phone.trusted())
                .ok_or_else(unavailable)?;
            let request =
                Box::new(ConsentRequest::new(plan.view.plan.clone()).map_err(GateError::invalid)?);
            (
                Surface::Phone {
                    facts: phone.clone(),
                    request: request.clone(),
                },
                ApprovalChallenge::Phone {
                    attempt: id,
                    connection: phone.connection,
                    correlation: id,
                    request,
                },
            )
        };
        self.attempts.insert(
            id,
            Attempt {
                digest: digest.clone(),
                caller: facts.connection,
                surface,
            },
        );
        Ok(challenge)
    }

    /// Complete under the commit lock after a fresh snapshot and active-surface
    /// lookup. The transport must match a response frame, not an unsolicited
    /// request. A wrong answer consumes this attempt, never the pending plan.
    pub fn finish_approval(
        &mut self,
        attempt: u64,
        reply: ApprovalReply,
        store: &mut ConfigStore,
        current: &GateSnapshot,
        facts: &ApprovalFacts,
        now_ms: u64,
    ) -> Result<u64> {
        let pending = self.attempts.remove(&attempt).ok_or_else(unavailable)?;
        if !facts.owner_uid || facts.connection != pending.caller {
            return Err(unavailable());
        }
        match (&pending.surface, reply) {
            (Surface::Desktop, ApprovalReply::Presence(decision)) => {
                if !facts.desktop() {
                    return Err(unavailable());
                }
                presence(decision)?;
            }
            (Surface::Os, ApprovalReply::Presence(decision)) => presence(decision)?,
            (
                Surface::Phone {
                    facts: sent,
                    request,
                },
                ApprovalReply::Phone {
                    connection,
                    correlation,
                    response,
                },
            ) => {
                let current = facts
                    .phone
                    .as_ref()
                    .filter(|phone| phone.trusted())
                    .ok_or_else(unavailable)?;
                if current != sent || connection != sent.connection || correlation != attempt {
                    return Err(unavailable());
                }
                let response = response.ok_or_else(declined)?;
                if !response.approves(request) {
                    return Err(declined());
                }
            }
            _ => return Err(unavailable()),
        }
        self.commit(&pending.digest, store, current, now_ms)
    }

    /// The owner wakes each waiting client with surface-unavailable when the
    /// supervised callosum connection exits or is replaced by a swap. An old
    /// phone card must not remain pending until its response deadline.
    pub fn invalidate_phone(&mut self, connection: u64) -> Vec<u64> {
        let ids: Vec<_> = self.attempts.iter().filter_map(|(id, attempt)| {
            matches!(&attempt.surface, Surface::Phone { facts, .. } if facts.connection == connection).then_some(*id)
        }).collect();
        for id in &ids {
            self.attempts.remove(id);
        }
        ids
    }
}

fn presence(decision: PresenceDecision) -> Result<()> {
    match decision {
        PresenceDecision::InteractiveApproval => Ok(()),
        PresenceDecision::Declined => Err(declined()),
        PresenceDecision::Unavailable => Err(unavailable()),
    }
}

/// Availability is a daemon observation (on Linux, test the per-user action
/// file as well as provider readiness); missing policy is never a silent grant.
pub fn missing_surfaces(facts: &ApprovalFacts, recovery: bool) -> Vec<String> {
    let mut missing = Vec::new();
    if !facts.desktop() {
        missing.push("desktop (required for stamp)".into());
    }
    if recovery || !facts.phone.as_ref().is_some_and(PhoneSurface::trusted) {
        missing.push(
            if recovery {
                "phone (unavailable in recovery)"
            } else {
                "phone"
            }
            .into(),
        );
    }
    if !facts.os_prompt_available {
        missing.push("OS prompt (Linux requires the per-user polkit action file)".into());
    }
    missing
}

/// A present policy is a prerequisite, not a promise that an interactive agent
/// is reachable. The provider's actual outcome remains authoritative.
pub fn os_prompt_policy_present() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&subc_presence::polkit::action_path(subc_os::effective_uid()))
            .is_file()
    }
    #[cfg(not(target_os = "linux"))]
    {
        cfg!(any(target_os = "macos", windows))
    }
}
