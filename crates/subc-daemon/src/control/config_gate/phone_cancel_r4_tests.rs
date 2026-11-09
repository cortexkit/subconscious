use super::*;

#[tokio::test]
async fn config_os_poison_after_precheck_and_wire_cancel_refuses_publication() {
    let fixture = cancel_r3_tests::os_fixture().await;
    let (reached, observed) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    fixture.owner.set_prepublication_hook(move || {
        let _ = reached.send(());
        wait.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
    });
    let apply = fixture.apply();
    tokio::time::timeout(std::time::Duration::from_secs(5), observed)
        .await
        .unwrap()
        .unwrap();
    fixture.owner.poison_approval_request_index_for_test();
    let replies = fixture
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
    assert_eq!(replies.len(), 1);
    assert_eq!(
        serde_json::from_slice::<subc_protocol::ErrorBody>(&replies[0].body)
            .unwrap()
            .code,
        "unknown_subscription"
    );
    release.send(()).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), apply)
        .await
        .unwrap()
        .unwrap();
    let history = fixture
        .owner
        .on_blocking(|owner| owner.history())
        .await
        .unwrap()
        .0
        .len();
    fixture.stop().await;
    assert!(matches!(&result, Err(error) if error.code == "config_unavailable"), "a cancellation index poisoned immediately before the publication claim must refuse: {result:?}; history={history}");
    assert_eq!(history, 1);
}

async fn reply(socket: &mut tokio::io::DuplexStream) -> Frame {
    tokio::time::timeout(std::time::Duration::from_secs(5), crate::read_frame(socket))
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn config_connection_loop_duplicate_correlation_cannot_cancel_the_reserved_apply() {
    let mut fixture = Fixture::new().await;
    let router = Arc::new(crate::Router::with_control_handler(fixture.handler.clone()));
    let (connection, mut socket, server) = crate::server::config_test_connection_loop(router);
    fixture.handler.observed_peers.lock().unwrap().insert(
        connection,
        subc_protocol::peer::ObservedPeer::observed_uid(subc_os::effective_uid()),
    );
    let (observed, release) = fixture.owner.pause_next_approval_claim_for_test();
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
    crate::write_frame(&mut socket, &apply).await.unwrap();
    // The first task owns the reader reservation but has not claimed it yet.
    // A duplicate must be refused, not open a card with that task's token.
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::select! {
            response = crate::read_frame(&mut socket) => Ok(response.unwrap().unwrap()),
            card = fixture.frames.recv() => Err(card.unwrap()),
        }
    })
    .await
    .unwrap();
    let (duplicate, applied, winner_cancelled) = match first {
        Ok(duplicate) => {
            release.send(()).unwrap();
            let (corr, request) = fixture.request().await;
            fixture.answer(PHONE, corr, &request);
            let applied =
                serde_json::from_slice::<ConfigControlResponse>(&reply(&mut socket).await.body)
                    .unwrap();
            (duplicate, Some(applied), false)
        }
        Err(card) => {
            // Keep the stolen card alive until cancellation is observed. Dropping
            // it would close the send_flushed acknowledgment channel and make
            // the test itself cause a writer failure.
            release.send(()).unwrap();
            let duplicate = reply(&mut socket).await;
            let cancel =
                tokio::time::timeout(std::time::Duration::from_secs(5), fixture.frames.recv())
                    .await
                    .unwrap()
                    .unwrap();
            let winner_cancelled = cancel.frame.header.ty == FrameType::Cancel
                && cancel.frame.header.corr == card.frame.header.corr;
            let withdrawn = reply(&mut socket).await;
            assert_eq!(
                serde_json::from_slice::<subc_protocol::ErrorBody>(&withdrawn.body)
                    .unwrap()
                    .code,
                "approval_surface_unavailable"
            );
            drop(card);
            (duplicate, None, winner_cancelled)
        }
    };
    let history = fixture
        .owner
        .on_blocking(|owner| owner.history())
        .await
        .unwrap()
        .0
        .len();
    drop(socket);
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    fixture.stop().await;
    assert_eq!(duplicate.header.corr, 881);
    assert_eq!(
        serde_json::from_slice::<subc_protocol::ErrorBody>(&duplicate.body)
            .unwrap()
            .code,
        "config_busy"
    );
    assert!(matches!(applied, Some(ConfigControlResponse::Applied { generation: 2 })),
        "a duplicate Apply must not steal the first task's token or let its losing guard cancel the winner; winner_cancelled={winner_cancelled}, applied={applied:?}, history={history}");
    assert_eq!(history, 2);
}
