use super::*;

/// A subscriber whose connection stops draining is dropped once its frame
/// channel fills. The client must learn that from a terminal Error frame
/// after the frames already queued for it, not from a stream that simply
/// goes quiet.
#[tokio::test]
async fn lagged_spawn_subscriber_receives_a_terminal_lagged_error_after_its_queued_frames() {
    let feed = SpawnEventFeed::default();
    feed.configure_incarnation("lag-incarnation".to_string());
    // A one-slot connection queue that nobody reads until the emits are
    // done: the forwarder parks on it and the subscriber channel fills.
    let (tx, mut rx) = mpsc::channel(1);
    feed.subscribe(ConnectionId::new(1), 7, 1, None, FrameSink::new(tx))
        .expect("subscribe");
    let emitted = SPAWN_SUBSCRIBER_BUFFER + 16;
    for index in 0..emitted {
        feed.emit_spawned(&format!("lag-module-{index}"), None, 1000, 0);
        // Let the forwarder take what it can so the fill point is the
        // subscriber channel, not a scheduling accident.
        tokio::task::yield_now().await;
    }
    assert_eq!(
        feed.subscriber_count(),
        0,
        "the lagged subscriber must be removed"
    );

    let mut data = Vec::new();
    let mut last = None;
    loop {
        let next = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the forwarder must finish once the subscriber is dropped");
        let Some(outbound) = next else { break };
        let frame = outbound.frame;
        if frame.header.ty == FrameType::StreamData {
            assert!(last.is_none(), "no data may follow the terminal frame");
            let event: SpawnEvent = serde_json::from_slice(&frame.body).unwrap();
            data.push(event.cursor.seq);
        } else {
            assert!(last.is_none(), "exactly one terminal frame");
            last = Some(frame);
        }
    }
    assert!(!data.is_empty(), "queued frames drain before the terminal");
    for pair in data.windows(2) {
        assert_eq!(
            pair[1],
            pair[0] + 1,
            "queued frames arrive dense and in order"
        );
    }
    let terminal = last.expect("a lagged subscriber must receive a terminal frame");
    assert_eq!(terminal.header.ty, FrameType::Error);
    assert_eq!(terminal.header.corr, 7);
    let body: subc_protocol::ErrorBody = serde_json::from_slice(&terminal.body).unwrap();
    assert_eq!(body.code, SPAWN_SUBSCRIBER_LAGGED_CODE);
    let detail = body.detail.expect("lagged error carries detail");
    assert_eq!(
        detail["first_undelivered_cursor"]["seq"],
        data.last().unwrap() + 1,
        "the named cursor is the first event the subscriber did not receive"
    );
    assert_eq!(
        detail["first_undelivered_cursor"]["daemon_incarnation"],
        "lag-incarnation"
    );
}
