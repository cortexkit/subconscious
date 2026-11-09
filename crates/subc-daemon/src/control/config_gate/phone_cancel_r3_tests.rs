use super::*;

struct ApprovedPresence;
impl crate::operator_confirm::OperatorProvider for ApprovedPresence {
    fn config_gate_ready(&self) -> bool {
        true
    }
    fn prompt(
        &self,
        _: &str,
        _: Box<dyn FnOnce(Arc<dyn crate::operator_confirm::OperatorWithdraw>) + Send>,
    ) -> crate::operator_confirm::ProviderResult {
        panic!("configuration must not enter the generic prompt")
    }
    fn prompt_gate(
        &self,
        _: &str,
        _: Box<dyn FnOnce(Arc<dyn crate::operator_confirm::OperatorWithdraw>) + Send>,
    ) -> crate::operator_confirm::ProviderResult {
        crate::operator_confirm::ProviderResult::Approved
    }
}

async fn os_fixture() -> Fixture {
    let mut fixture = Fixture::new().await;
    fixture.plan = fixture
        .owner
        .finish_plan(
            fixture.owner.prepare(PlanKind::Change).unwrap(),
            |old, new| {
                let mut changes = daemon_review(old, new)?;
                let mut field = subc_config::FieldChange::new(
                    "a",
                    "/limit",
                    subc_config::RecordedValue::Value { value: json!(1) },
                    subc_config::RecordedValue::Value { value: json!(2) },
                    subc_config::ApplyKind::Live,
                );
                field.old_source = subc_config::Layer::User;
                field.new_source = subc_config::Layer::User;
                changes.fields = vec![field];
                Ok(changes)
            },
        )
        .unwrap();
    fixture.owner.set_os_prompt_policy_for_tests();
    fixture
        .handler
        .forwarding
        .operator_confirms()
        .configure(Arc::new(ApprovedPresence), Default::default());
    let facts = fixture
        .handler
        .config_approval_facts(&RouteCtx {
            connection_id: CALLER,
            egress: crate::FrameSink::new(mpsc::channel(1).0),
        })
        .await;
    let challenge = fixture
        .owner
        .begin_approval(&fixture.plan.digest, &facts)
        .unwrap();
    assert!(matches!(challenge, ApprovalChallenge::OsPrompt { .. }));
    fixture.owner.cancel_approval(challenge.attempt());
    fixture
}

enum Withdraw {
    Abort,
    Disconnect,
    Poison,
    WireCancel,
}

async fn after_last_drain(action: Withdraw) {
    let fixture = os_fixture().await;
    let (reached, observed) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    fixture.owner.set_after_approval_drain_hook(move || {
        let _ = reached.send(());
        wait.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
    });
    let apply = fixture.apply();
    tokio::time::timeout(std::time::Duration::from_secs(5), observed)
        .await
        .unwrap()
        .unwrap();
    let aborted = matches!(action, Withdraw::Abort);
    match action {
        Withdraw::Abort => apply.abort(),
        Withdraw::Disconnect => {
            fixture.handler.cleanup_connection(CALLER).unwrap();
        }
        Withdraw::Poison => fixture.owner.poison_approval_request_index_for_test(),
        Withdraw::WireCancel => {
            let responses = fixture
                .handler
                .handle_control_frame(
                    &RouteCtx {
                        connection_id: CALLER,
                        egress: crate::FrameSink::new(mpsc::channel(1).0),
                    },
                    Frame::build(FrameType::Cancel, control_flags(), 0, 0, 881, vec![]).unwrap(),
                )
                .await
                .unwrap();
            assert!(responses.is_empty());
        }
    }
    let result = if aborted {
        assert!(apply.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        None
    } else {
        release.send(()).unwrap();
        Some(
            tokio::time::timeout(std::time::Duration::from_secs(5), apply)
                .await
                .unwrap()
                .unwrap(),
        )
    };
    let history = fixture
        .owner
        .on_blocking(|owner| owner.history())
        .await
        .unwrap()
        .0
        .len();
    fixture.stop().await;
    if let Some(result) = result {
        let code = if matches!(action, Withdraw::Poison) {
            "config_unavailable"
        } else {
            "approval_surface_unavailable"
        };
        assert!(
            matches!(&result, Err(error) if error.code == code),
            "withdrawal after the final drain must refuse publication: {result:?}"
        );
    }
    assert_eq!(history, 1, "OS approval must retain the exact request's cancellation witness after it leaves the index");
}

#[tokio::test]
async fn config_os_abort_after_last_drain_never_publishes() {
    after_last_drain(Withdraw::Abort).await;
}
#[tokio::test]
async fn config_os_disconnect_after_last_drain_never_publishes() {
    after_last_drain(Withdraw::Disconnect).await;
}
#[tokio::test]
async fn config_os_poisoned_cancellation_index_refuses_publication() {
    after_last_drain(Withdraw::Poison).await;
}
#[tokio::test]
async fn config_os_wire_cancel_after_presence_answer_is_not_swallowed() {
    after_last_drain(Withdraw::WireCancel).await;
}

#[tokio::test]
async fn config_connection_loop_refused_applies_release_all_reservations() {
    let fixture = Fixture::new().await;
    let baseline = fixture.owner.approval_request_count_for_test();
    let router = Arc::new(crate::Router::with_control_handler(fixture.handler.clone()));
    let (_connection, mut socket, server) = crate::server::config_test_connection_loop(router);
    for corr in 1000..1016 {
        let frame = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            corr,
            serde_json::to_vec(&ConfigControlRequest::Apply {
                digest: fixture.plan.digest.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        crate::write_frame(&mut socket, &frame).await.unwrap();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            crate::read_frame(&mut socket),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(response.header.corr, corr);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&response.body).unwrap()["code"],
            "owner_required"
        );
    }
    let deadline = Instant::now() + std::time::Duration::from_secs(1);
    while fixture.owner.approval_request_count_for_test() != baseline && Instant::now() < deadline {
        tokio::task::yield_now().await;
    }
    let remaining = fixture.owner.approval_request_count_for_test();
    drop(socket);
    server.await.unwrap().unwrap();
    fixture.stop().await;
    assert_eq!(
        remaining, baseline,
        "refused applies must release reservations before disconnect"
    );
}

#[tokio::test]
async fn config_apply_reservation_is_owned_before_the_dispatch_task_first_polls() {
    let fixture = Fixture::new().await;
    let baseline = fixture.owner.approval_request_count_for_test();
    let reservation = fixture.owner.reserve_approval_request(CALLER, 1234);
    assert_eq!(
        fixture.owner.approval_request_count_for_test(),
        baseline + 1
    );
    let task = async move {
        let _reservation = reservation;
        std::future::pending::<()>().await;
    };
    drop(task);
    let remaining = fixture.owner.approval_request_count_for_test();
    fixture.stop().await;
    assert_eq!(
        remaining, baseline,
        "an unpolled dispatch future must own reservation cleanup"
    );
}

#[tokio::test]
async fn config_phone_failed_bind_never_cancels_an_unsent_card() {
    let mut fixture = Fixture::new().await;
    let (reached, observed) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    fixture.owner.set_phone_bind_hook(move || {
        let _ = reached.send(());
        wait.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
    });
    let apply = fixture.apply();
    tokio::time::timeout(std::time::Duration::from_secs(5), observed)
        .await
        .unwrap()
        .unwrap();
    assert!(fixture.owner.cancel_approval_request(CALLER, 881));
    release.send(()).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), apply)
        .await
        .unwrap()
        .unwrap();
    let unsent = fixture.frames.try_recv().is_err();
    let removed = fixture
        .handler
        .forwarding
        .pending_module_control_op(PHONE, 1)
        .unwrap()
        .is_none();
    fixture.stop().await;
    assert!(matches!(result, Err(error) if error.code == "approval_surface_unavailable"));
    assert!(
        removed,
        "failed bind must still clean up the allocated control RPC"
    );
    assert!(
        unsent,
        "no channel-zero Cancel is valid before its card was ever enqueued"
    );
}
