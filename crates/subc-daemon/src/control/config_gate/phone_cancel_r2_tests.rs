use super::*;

#[tokio::test]
async fn config_phone_cancel_during_bind_removes_the_allocated_control_rpc() {
    let fixture = Fixture::new().await;
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
    assert_eq!(
        fixture
            .handler
            .forwarding
            .pending_module_control_op(PHONE, 1)
            .unwrap()
            .as_deref(),
        Some(subc_protocol::consent::CONSENT_V1)
    );
    apply.abort();
    assert!(apply.await.unwrap_err().is_cancelled());
    let removed = fixture
        .handler
        .forwarding
        .pending_module_control_op(PHONE, 1)
        .unwrap()
        .is_none();
    release.send(()).unwrap();
    fixture.stop().await;
    assert!(
        removed,
        "cancelling a phone bind must remove its already allocated forwarding control-RPC entry"
    );
}

#[tokio::test]
async fn config_connection_loop_disconnect_during_begin_returns_slots_for_a_fifth_attempt() {
    let fixture = Fixture::new().await;
    let router = Arc::new(crate::Router::with_control_handler(fixture.handler.clone()));
    for _ in 0..4 {
        let before = fixture.owner.begin_approval_completions_for_test();
        let (reached, observed) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        fixture.owner.set_begin_approval_hook(move || {
            let _ = reached.send(());
            wait.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
        });
        let (connection, mut socket, server) =
            crate::server::config_test_connection_loop(router.clone());
        fixture.handler.observed_peers.lock().unwrap().insert(
            connection,
            subc_protocol::peer::ObservedPeer::observed_uid(subc_os::effective_uid()),
        );
        let apply = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            881,
            serde_json::to_vec(&ConfigControlRequest::Apply {
                digest: fixture.plan.digest.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        crate::write_frame(&mut socket, &apply).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), observed)
            .await
            .unwrap()
            .unwrap();
        drop(socket);
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        release.send(()).unwrap();
        // The hook is before the owner lock: real connection cleanup has
        // already finished before the old worker can insert its late slot.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while fixture.owner.begin_approval_completions_for_test() == before {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    let facts = fixture
        .handler
        .config_approval_facts(&RouteCtx {
            connection_id: CALLER,
            egress: crate::FrameSink::new(mpsc::channel(1).0),
        })
        .await;
    let fifth = fixture.owner.begin_approval(&fixture.plan.digest, &facts);
    if let Ok(challenge) = &fifth {
        fixture.owner.cancel_approval(challenge.attempt());
    }
    assert_eq!(fixture.owner.history().unwrap().0.len(), 1);
    fixture.stop().await;
    assert!(fifth.is_ok(), "four disconnected begin jobs must return their slots so a fifth approval can start: {fifth:?}");
}

#[tokio::test]
async fn config_phone_wire_cancel_after_drain_before_publication_records_nothing() {
    cancel_at_publication_boundary(false).await;
}

#[tokio::test]
async fn config_phone_wire_cancel_after_publication_claim_records_too_late_and_reports_outcome() {
    cancel_at_publication_boundary(true).await;
}

async fn cancel_at_publication_boundary(after_claim: bool) {
    let mut fixture = Fixture::new().await;
    fixture.handler = Arc::new(
        fixture
            .handler
            .as_ref()
            .clone()
            .with_activity_ledger(&fixture.scratch.0, "cancel-boundary".into()),
    );
    let (reached, observed) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let hook = move || {
        let _ = reached.send(());
        wait.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
    };
    if after_claim {
        fixture.owner.set_publication_hook(hook);
    } else {
        fixture.owner.set_prepublication_hook(hook);
    }
    let apply = fixture.apply();
    let (corr, request) = fixture.request().await;
    fixture.answer(PHONE, corr, &request);
    tokio::time::timeout(std::time::Duration::from_secs(5), observed)
        .await
        .unwrap()
        .unwrap();
    let cancel = Frame::build(FrameType::Cancel, control_flags(), 0, 0, 881, vec![]).unwrap();
    assert!(
        fixture
            .handler
            .handle_control_frame(
                &RouteCtx {
                    connection_id: CALLER,
                    egress: crate::FrameSink::new(mpsc::channel(1).0),
                },
                cancel
            )
            .await
            .unwrap()
            .is_empty(),
        "wire Cancel keeps its no-reply contract"
    );
    release.send(()).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), apply)
        .await
        .unwrap()
        .unwrap();
    let history = fixture.owner.history().unwrap().0.len();
    let ledger = fixture.handler.activity.as_ref().unwrap();
    ledger
        .record(
            "barrier".into(),
            serde_json::from_value(json!({
                "op":"activity.record", "kind":"barrier.flushed", "summary":"Queue flushed"
            }))
            .unwrap(),
            subc_control::ActivityActor {
                principal: subc_protocol::Principal::Direct,
                scope_owner: None,
                agent_id: None,
                run_id: None,
                flow_id: None,
                project_root: None,
                workdir: None,
                project_id: None,
            },
            1,
        )
        .await
        .unwrap();
    let entries = ledger
        .query(serde_json::from_value(json!({"op":"activity.query", "limit":1000})).unwrap())
        .await
        .unwrap()
        .entries;
    let late: Vec<_> = entries
        .iter()
        .filter(|entry| entry.kind == "config.cancel_too_late")
        .collect();
    let correct_outcome = if after_claim {
        matches!(
            &result,
            Ok(ConfigControlResponse::Applied { generation: 2, .. })
        ) && history == 2
    } else {
        matches!(&result, Err(error) if error.code == "approval_surface_unavailable")
            && history == 1
    };
    let correct_activity = if after_claim {
        late.len() == 1 && serde_json::to_value(late[0]).unwrap()["details"]["correlation"] == 881
    } else {
        late.is_empty()
    };
    fixture.stop().await;
    assert!(correct_outcome, "Cancel must win before publication and report the real outcome afterward: after_claim={after_claim}, history={history}, result={result:?}");
    assert!(correct_activity, "post-claim Cancel must record config.cancel_too_late with its caller correlation, never a new wire reply");
}
