//! After HELLO, each spawn files at most one review card listing all differing
//! provider keys and their exact requested entries. Approval requires a reply
//! from the same still-trusted Callosum connection and wire correlation to which
//! the gate sent the card; an unsolicited answer cannot persist entry approvals.

use super::*;
use crate::spawn_authority::SpawnAuthorityRecord;
use subc_protocol::provider_declarations::ProviderDeclarations;

impl ControlHandler {
    pub(super) fn publish_development_approval(
        &self,
        approval: &crate::program_origin::DevelopmentApproval,
    ) {
        for record in self
            .supervisor
            .spawn_authority_records_for_module(&approval.module_id)
        {
            if record
                .capture()
                .is_some_and(|capture| capture.incarnation == approval.incarnation)
            {
                self.approve_spawn_declarations(&record, &approval.requested_entries);
            }
        }
    }
    pub(crate) fn file_development_widening(
        &self,
        record: Option<Arc<SpawnAuthorityRecord>>,
        entries: ProviderDeclarations,
    ) {
        let Some(record) = record.filter(|_| !entries.is_empty()) else {
            return;
        };
        let Some(runtime) = self.config_runtime.clone() else {
            return;
        };
        let handler = self.clone();
        tokio::spawn(async move {
            if let Err(error) = handler
                .request_development_widening(runtime, record.clone(), entries)
                .await
            {
                info!(
                    module_id = record.module_id(),
                    spawn = record.spawn(),
                    code = error.code,
                    "development provider declarations remain unapproved"
                );
            }
        });
    }

    async fn request_development_widening(
        &self,
        runtime: Arc<crate::config_gate::runtime::RuntimeOwner>,
        record: Arc<SpawnAuthorityRecord>,
        entries: ProviderDeclarations,
    ) -> std::result::Result<(), GateError> {
        let connection = self
            .forwarding
            .connection_for_spawn(&record.key())
            .map_err(|error| GateError::new("config_unavailable", error))?
            .ok_or_else(|| GateError::new("stale_plan", "spawn registration ended"))?;
        let request_guard = runtime.claim_approval_request(connection, 0)?;
        let plan_record = record.clone();
        let view = config_work(runtime.clone(), move |owner| {
            owner.plan_development_widening(&plan_record, entries)
        })
        .await?;
        let facts = self.development_approval_facts(connection).await;
        let cancellation = request_guard.request.clone();
        let digest = view.digest;
        let (challenge, mut attempt_guard) = config_work(runtime.clone(), move |owner| {
            let challenge =
                owner.begin_approval_for_request(&digest, &facts, Some(&cancellation))?;
            let guard = phone::ApprovalAttemptGuard::new(&owner, challenge.attempt());
            owner.track_reserved_approval_request(
                challenge.attempt(),
                connection,
                0,
                &cancellation,
            )?;
            Ok((challenge, guard))
        })
        .await?;
        let ApprovalChallenge::Phone {
            attempt,
            connection: phone_connection,
            deadline,
            request,
        } = challenge
        else {
            return Err(GateError::new(
                "approval_surface_unavailable",
                "development entries require a complete review card",
            ));
        };
        let reply = self
            .config_phone_rpc(
                &runtime,
                attempt,
                ConnectionId::new(phone_connection),
                deadline,
                &request,
            )
            .await?;
        let window = Arc::new(crate::config_gate::runtime::ApprovalCommitWindow::new(
            Instant::now() + CONFIG_APPROVAL_COMMIT_TIMEOUT,
        ));
        attempt_guard.commit_window = Some(window.clone());
        let locks = async {
            let operation = self.supervisor.operation_lock().lock_owned().await;
            let gates = runtime.approval_commit_gates(attempt).await?;
            let facts = self.development_approval_facts(connection).await;
            Ok::<_, GateError>((operation, gates, facts))
        };
        let (operation, gates, facts) = timeout_at(window.deadline, locks)
            .await
            .map_err(|_| GateError::commit_timeout())??;
        if !record.alive() {
            return Err(GateError::new("stale_plan", "spawn ended before approval"));
        }
        let publication = request_guard.request.clone();
        let generation = config_work(runtime.clone(), move |owner| {
            let _gates = (operation, gates);
            owner.finish_approval_with_commit_window(
                attempt,
                &facts,
                reply,
                now_ms(),
                Some(window),
                publication,
            )
        })
        .await?;
        if let Some(approval) =
            config_work(runtime, move |owner| owner.development_approval(generation)).await?
        {
            self.publish_development_approval(&approval);
        }
        Ok(())
    }

    async fn development_approval_facts(&self, connection: ConnectionId) -> ApprovalFacts {
        ApprovalFacts {
            connection: connection.get(),
            owner_uid: true,
            desktop_app: false,
            attested_operator_app: false,
            os_prompt_available: false,
            phone: self.config_phone_surface().await,
            caller: Default::default(),
        }
    }
}
