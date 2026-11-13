//! A person approves exact development provider entries through the gate's
//! digest-bound plan and existing full desktop review or phone consent flow.
//! Entry approvals never change configuration delivery or its active pointers.

use super::*;
use crate::{program_origin::DevelopmentApproval, spawn_authority::SpawnAuthorityRecord};
use subc_protocol::provider_declarations::ProviderDeclarations;

impl RuntimeOwner {
    pub(crate) fn plan_development_widening(
        &self,
        record: &SpawnAuthorityRecord,
        requested_entries: ProviderDeclarations,
    ) -> Result<ConfigPlanView> {
        let mut state = self.lock()?;
        Self::check_authority(&mut state, true)?;
        let observed = self.capture(&mut state)?.diagnostic;
        let OwnerState { gate, store, .. } = &mut *state;
        let view = gate.plan_development_widening(store, record, requested_entries, &observed)?;
        state.reviewed(&view.digest, Instant::now());
        Ok(view)
    }

    pub(crate) fn development_approval(
        &self,
        generation: u64,
    ) -> Result<Option<DevelopmentApproval>> {
        let mut state = self.lock()?;
        let entry = state
            .store
            .body()?
            .generations
            .get(&generation)
            .ok_or_else(GateError::stale)?;
        if entry.kind != crate::config_store::CommitKind::DeclarationApproval {
            return Ok(None);
        }
        serde_json::from_value(
            entry.generation.changeset.apply_plan["review"]["development_widening"].clone(),
        )
        .map(Some)
        .map_err(GateError::invalid)
    }
}

impl super::super::ConfigGate {
    pub(crate) fn plan_development_widening(
        &mut self,
        store: &mut crate::config_store::ConfigStore,
        record: &SpawnAuthorityRecord,
        requested_entries: ProviderDeclarations,
        observed: &super::super::GateSnapshot,
    ) -> Result<ConfigPlanView> {
        let capture = record
            .capture()
            .filter(|capture| capture.origin == subc_protocol::peer::CallerOrigin::Development)
            .ok_or_else(GateError::stale)?;
        let approval = DevelopmentApproval {
            module_id: record.module_id().into(),
            incarnation: capture.incarnation,
            spawn: record.spawn(),
            origin: capture.origin,
            requested_entries,
        };
        let candidate = super::super::GateSnapshot::from_store(&store.body()?.approved)?;
        let card = approval.clone();
        let qualifies = record.origin_qualifies_for_mark();
        let view = self.plan(
            store,
            super::super::PlanKind::Change,
            candidate,
            observed,
            move |_, _| {
                let mut changeset = Changeset::new();
                changeset.launch_manifests.insert(
                    card.module_id.clone(),
                    serde_json::json!({
                        "origin": "development", "origin_qualifies_for_mark": qualifies,
                        "provider_declarations": card.requested_entries,
                    }),
                );
                changeset.apply_plan = serde_json::json!({"development_widening": card});
                Ok(changeset)
            },
        )?;
        self.plans
            .get_mut(&view.digest)
            .expect("new pending plan")
            .development_approval = Some(approval);
        Ok(view)
    }
}
