use super::*;
use crate::{ActivityEntry, ActivityRefusal, ActivityStreamError, ActivitySubscribeResponse};
use serde_json::json;

fn consumer() -> (SubcConsumer, mpsc::Receiver<WriteCommand>) {
    let shared = Arc::new(Shared::new(
        PathBuf::from("unused-connection.json"),
        ConsumerOptions::default(),
    ));
    let (writer, wire) = mpsc::channel(16);
    shared.lock_inner().writer = Some(writer);
    (SubcConsumer { shared }, wire)
}

fn entry() -> ActivityEntry {
    serde_json::from_value(json!({
        "seq":42, "at_ms":12, "daemon_incarnation":"daemon-1", "module":"reporter",
        "kind":"reporter.changed", "summary":"Changed package id", "details":{"run_id":"run-1"},
        "actor":{"principal":{"kind":"reserved", "module_id":"reporter"}},
    }))
    .unwrap()
}

fn query() -> ActivityQueryRequest {
    serde_json::from_value(json!({
        "op":"activity.query", "agent":"agent-1", "project":"project-1", "module":"reporter",
        "kind":"reporter.changed", "since_ms":10, "until_ms":20, "limit":7, "cursor":99,
    }))
    .unwrap()
}

fn frame(corr: u64, ty: FrameType, body: impl serde::Serialize) -> Frame {
    Frame::build(
        ty,
        Flags::new(true, Priority::Interactive, false),
        0,
        0,
        corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn activity_query_preserves_filters_entries_cursor_and_head() {
    let (consumer, mut wire) = consumer();
    let caller = SubcConsumer {
        shared: Arc::clone(&consumer.shared),
    };
    let call = tokio::spawn(async move { caller.query_activity(query()).await });
    let request = wire.recv().await.unwrap().frame;
    assert_eq!(request.header.channel, 0);
    assert_eq!(
        serde_json::from_slice::<ActivityQueryRequest>(&request.body).unwrap(),
        query()
    );
    let expected = ActivityQueryResponse {
        entries: vec![entry()],
        cursor: Some(42),
        head: ActivityCursor {
            daemon_incarnation: "daemon-1".into(),
            seq: 55,
        },
    };
    assert!(
        dispatch_frame(
            &consumer.shared,
            1,
            frame(request.header.corr, FrameType::Response, &expected)
        )
        .await
    );
    assert_eq!(call.await.unwrap().unwrap(), expected);
}

#[tokio::test]
async fn activity_query_preserves_daemon_refusal_code() {
    let (consumer, mut wire) = consumer();
    let caller = SubcConsumer {
        shared: Arc::clone(&consumer.shared),
    };
    let call = tokio::spawn(async move { caller.query_activity(query()).await });
    let request = wire.recv().await.unwrap().frame;
    dispatch_frame(
        &consumer.shared,
        1,
        frame(
            request.header.corr,
            FrameType::Error,
            ErrorBody::new("activity_read_forbidden", "operator only"),
        ),
    )
    .await;
    assert_eq!(
        call.await.unwrap().unwrap_err().code(),
        Some("activity_read_forbidden")
    );
}

#[tokio::test]
async fn activity_subscribe_yields_entry_before_cursor_and_lag_errors_with_original_codes() {
    let cases = [
        (
            "activity_cursor_incarnation_mismatch",
            ActivityRefusal::CursorIncarnationMismatch,
        ),
        ("activity_cursor_too_old", ActivityRefusal::CursorTooOld),
        (
            "activity_subscriber_lagged",
            ActivityRefusal::SubscriberLagged,
        ),
        ("activity_read_forbidden", ActivityRefusal::ReadForbidden),
        (
            "activity_future_refusal",
            ActivityRefusal::Other("activity_future_refusal".into()),
        ),
    ];
    for (code, reason) in cases {
        let (consumer, mut wire) = consumer();
        let since = ActivityCursor {
            daemon_incarnation: "daemon-1".into(),
            seq: 41,
        };
        let mut stream = consumer
            .subscribe_activity(Some(since.clone()))
            .await
            .unwrap();
        let request = wire.recv().await.unwrap().frame;
        assert_eq!(request.header.channel, 0);
        assert_eq!(
            serde_json::from_slice::<ActivitySubscribeRequest>(&request.body)
                .unwrap()
                .since,
            Some(since)
        );
        dispatch_frame(
            &consumer.shared,
            1,
            frame(
                request.header.corr,
                FrameType::StreamData,
                ActivitySubscribeResponse { entry: entry() },
            ),
        )
        .await;
        let body = ErrorBody::new(code, "feed ended").with_detail(json!({"seq":43}));
        dispatch_frame(
            &consumer.shared,
            1,
            frame(request.header.corr, FrameType::Error, &body),
        )
        .await;
        assert_eq!(stream.next().await.unwrap(), Some(entry()));
        let error = stream.next().await.unwrap_err();
        assert_eq!(error.code(), Some(code));
        match error {
            ActivityStreamError::Refused {
                reason: actual,
                body: actual_body,
            } => {
                assert_eq!(actual, reason);
                assert_eq!(*actual_body, body);
            }
            other => panic!("expected typed stream refusal: {other:?}"),
        }
        assert!(stream.next().await.unwrap().is_none());
    }
}

#[tokio::test]
async fn activity_subscribe_decode_error_and_unsubscribe_end_the_stream() {
    let (consumer, mut wire) = consumer();
    let mut stream = consumer.subscribe_activity(None).await.unwrap();
    let request = wire.recv().await.unwrap().frame;
    dispatch_frame(
        &consumer.shared,
        1,
        frame(
            request.header.corr,
            FrameType::StreamData,
            json!({"op":"wrong"}),
        ),
    )
    .await;
    assert!(matches!(
        stream.next().await,
        Err(ActivityStreamError::Decode(_))
    ));
    assert!(stream.next().await.unwrap().is_none());
    // Consume the cancellation caused by the invalid entry before the next request.
    assert_eq!(
        wire.recv().await.unwrap().frame.header.ty,
        FrameType::Cancel
    );
    let mut stream = consumer.subscribe_activity(None).await.unwrap();
    wire.recv().await.unwrap();
    stream.unsubscribe().unwrap();
    assert!(stream.next().await.unwrap().is_none());
    assert_eq!(
        wire.recv().await.unwrap().frame.header.ty,
        FrameType::Cancel
    );
}
