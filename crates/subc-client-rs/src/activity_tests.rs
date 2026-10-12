use subc_protocol::{Flags, ModuleHelloAckBody, PROTOCOL_VERSION};
use tokio::{sync::mpsc, time::timeout};
use tokio_util::sync::CancellationToken;

use super::*;

fn module(capability: bool) -> (ModuleHandle, mpsc::Receiver<Frame>) {
    let ack = ModuleHelloAckBody {
        negotiated_ver: PROTOCOL_VERSION,
        subc_ops: vec![subc_control::ops::ACTIVITY_RECORD.to_owned()],
        subc_capabilities: if capability {
            vec![ACTIVITY_LEDGER_CAPABILITY.to_owned()]
        } else {
            vec![]
        },
        storage: None,
        machine_id: None,
    };
    let (tx, rx) = mpsc::channel(4);
    (
        ModuleHandle::new(
            &ack,
            tx,
            1,
            CancellationToken::new(),
            tokio::runtime::Handle::current(),
        ),
        rx,
    )
}

fn request() -> ActivityRecordRequest {
    ActivityRecordRequest {
        kind: "reporter.changed".into(),
        summary: "Updated package id".into(),
        subject: Some(ActivitySubject {
            subject_type: "package".into(),
            id: "package-1".into(),
        }),
        details: Some(serde_json::from_value(serde_json::json!({"run_id":"run-1"})).unwrap()),
        on: Some(ActivityOn::Route(ActivityRoute {
            channel: 7,
            epoch: 2,
        })),
    }
}

fn reply(request: &Frame, ty: FrameType, body: Vec<u8>) -> Frame {
    Frame::build_with_version(
        PROTOCOL_VERSION,
        ty,
        Flags::new(true, Priority::Interactive, false),
        0,
        0,
        request.header.corr,
        body,
    )
    .unwrap()
}
use subc_protocol::Priority;

#[tokio::test]
async fn activity_record_requires_capability_even_when_op_is_advertised() {
    let (handle, mut wire) = module(false);
    assert!(matches!(
        handle.record_activity(request()).await,
        Err(ActivityRecordError::NotSupported)
    ));
    assert!(
        wire.try_recv().is_err(),
        "unsupported recording must not send a frame"
    );
}

#[tokio::test]
async fn activity_record_sends_typed_body_and_returns_daemon_sequence() {
    let (handle, mut wire) = module(true);
    let worker = handle.clone();
    let call = tokio::spawn(async move { worker.record_activity(request()).await });
    let sent = wire.recv().await.unwrap();
    assert_eq!(sent.header.channel, 0);
    assert_eq!(
        serde_json::from_slice::<ActivityRecordRequest>(&sent.body).unwrap(),
        request()
    );
    assert!(handle.handle_control_reply(reply(
        &sent,
        FrameType::Response,
        br#"{"op":"activity.record","seq":42}"#.to_vec()
    )));
    assert_eq!(call.await.unwrap().unwrap().seq, 42);
}

#[tokio::test]
async fn activity_record_classifies_every_daemon_refusal_and_keeps_unknown_codes() {
    let (handle, mut wire) = module(true);
    let cases = [
        ("activity_kind_forbidden", ActivityRefusal::KindForbidden),
        ("activity_actor_unknown", ActivityRefusal::ActorUnknown),
        ("activity_rate_limited", ActivityRefusal::RateLimited),
        ("activity_read_forbidden", ActivityRefusal::ReadForbidden),
        ("activity_too_large", ActivityRefusal::TooLarge),
        ("activity_malformed", ActivityRefusal::Malformed),
        ("activity_unavailable", ActivityRefusal::Unavailable),
        (
            "activity_cursor_incarnation_mismatch",
            ActivityRefusal::CursorIncarnationMismatch,
        ),
        ("activity_cursor_too_old", ActivityRefusal::CursorTooOld),
        (
            "activity_subscriber_lagged",
            ActivityRefusal::SubscriberLagged,
        ),
        (
            "activity_future_refusal",
            ActivityRefusal::Other("activity_future_refusal".into()),
        ),
    ];
    for (code, expected) in cases {
        let worker = handle.clone();
        let call = tokio::spawn(async move { worker.record_activity(request()).await });
        let sent = wire.recv().await.unwrap();
        let body = ErrorBody::new(code, "refused").with_detail(serde_json::json!({"retry_ms":100}));
        assert!(handle.handle_control_reply(reply(
            &sent,
            FrameType::Error,
            serde_json::to_vec(&body).unwrap()
        )));
        let error = call.await.unwrap().unwrap_err();
        assert_eq!(error.code(), Some(code));
        match error {
            ActivityRecordError::Refused {
                reason,
                body: actual,
            } => {
                assert_eq!(reason, expected);
                assert_eq!(reason.code(), code);
                assert_eq!(*actual, body);
            }
            other => panic!("expected typed refusal, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn activity_record_rejects_wrong_response_op() {
    let (handle, mut wire) = module(true);
    let worker = handle.clone();
    let call = tokio::spawn(async move { worker.record_activity(request()).await });
    let sent = wire.recv().await.unwrap();
    handle.handle_control_reply(reply(
        &sent,
        FrameType::Response,
        br#"{"op":"activity.query","seq":42}"#.to_vec(),
    ));
    assert!(matches!(
        call.await.unwrap(),
        Err(ActivityRecordError::Protocol(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn activity_record_timeout_cancel_and_connection_close_release_waiters() {
    let (handle, mut wire) = module(true);
    let worker = handle.clone();
    let call = tokio::spawn(async move { worker.record_activity(request()).await });
    wire.recv().await.unwrap();
    tokio::time::advance(CATALOG_UPDATE_TIMEOUT).await;
    assert!(matches!(
        call.await.unwrap(),
        Err(ActivityRecordError::Timeout)
    ));
    assert!(handle
        .shared
        .lock_inner()
        .pending_activity_records
        .is_empty());
    let worker = handle.clone();
    let call = tokio::spawn(async move { worker.record_activity(request()).await });
    wire.recv().await.unwrap();
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());
    assert!(handle
        .shared
        .lock_inner()
        .pending_activity_records
        .is_empty());
    let worker = handle.clone();
    let call = tokio::spawn(async move { worker.record_activity(request()).await });
    wire.recv().await.unwrap();
    handle.close_connection();
    assert!(matches!(
        timeout(std::time::Duration::from_secs(1), call)
            .await
            .unwrap()
            .unwrap(),
        Err(ActivityRecordError::ConnectionClosed)
    ));
    assert!(handle
        .shared
        .lock_inner()
        .pending_activity_records
        .is_empty());
}
