use super::*;
use serde_json::json;

fn fake_consumer() -> (Arc<SubcConsumer>, mpsc::Receiver<WriteCommand>) {
    let shared = Arc::new(Shared::new(PathBuf::new(), ConsumerOptions::default()));
    let (writer, rx) = mpsc::channel(1);
    shared.lock_inner().writer = Some(writer);
    (Arc::new(SubcConsumer { shared }), rx)
}

async fn answer(
    consumer: &SubcConsumer,
    request: WriteCommand,
    ty: FrameType,
    body: serde_json::Value,
) {
    assert_eq!(request.frame.header.channel, 0);
    let frame = Frame::build(
        ty,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        request.frame.header.corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    assert!(dispatch_frame(&consumer.shared, 1, frame).await);
}

#[tokio::test]
async fn describe_connection_decodes_facts_and_only_unknown_control_op_is_older() {
    for (ty, body) in [
        (
            FrameType::Response,
            json!({"op":"connection.describe", "local":true, "operator_app":{"app":"fixture", "pid":42}}),
        ),
        (
            FrameType::Error,
            json!({"code":"unknown_control_op", "message":"unknown client control op"}),
        ),
        (
            FrameType::Error,
            json!({"code":"unsupported_control_frame", "message":"not an older client dispatcher"}),
        ),
        (
            FrameType::Error,
            json!({"code":"invalid_control_body", "message":"bad request"}),
        ),
        (FrameType::Response, json!({"op":"app.open", "local":true})),
    ] {
        let (consumer, mut rx) = fake_consumer();
        let call_consumer = consumer.clone();
        let call = tokio::spawn(async move { call_consumer.describe_connection().await });
        let request = rx.recv().await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&request.frame.body).unwrap(),
            json!({"op":"connection.describe"})
        );
        answer(&consumer, request, ty, body.clone()).await;
        let result = call.await.unwrap();
        match body["code"].as_str() {
            Some("unknown_control_op") => assert!(result.unwrap().is_none()),
            Some(code) => assert_eq!(result.unwrap_err().code(), Some(code)),
            None if body["op"] == "connection.describe" => {
                let facts = result.unwrap().unwrap();
                assert!(facts.local());
                assert_eq!(
                    facts.operator_app(),
                    Some(&subc_protocol::peer::OperatorApp::new("fixture", 42))
                );
            }
            None => assert_eq!(
                result.unwrap_err().not_sent_cause(),
                Some(NotSentCause::InvalidResponse)
            ),
        }
    }
}

#[tokio::test]
async fn app_open_preserves_successes_future_statuses_and_all_daemon_refusals() {
    for status in ["launched", "already_running", "future_status"] {
        let (consumer, mut rx) = fake_consumer();
        let caller = consumer.clone();
        let call = tokio::spawn(async move { caller.app_open("fixture").await });
        let request = rx.recv().await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&request.frame.body).unwrap(),
            json!({"op":"app.open", "app":"fixture"})
        );
        answer(
            &consumer,
            request,
            FrameType::Response,
            json!({"op":"app.open", "status":status, "pid":4242}),
        )
        .await;
        let reply = call.await.unwrap().unwrap();
        assert_eq!(serde_json::to_value(&reply).unwrap()["status"], status);
        assert_eq!(reply.pid, 4242);
    }
    for code in subc_control::APP_OPEN_ERROR_CODES
        .into_iter()
        .chain(["future_refusal", "unknown_control_op"])
    {
        let (consumer, mut rx) = fake_consumer();
        let caller = consumer.clone();
        let call = tokio::spawn(async move { caller.app_open("fixture").await });
        answer(
            &consumer,
            rx.recv().await.unwrap(),
            FrameType::Error,
            json!({"code":code, "message":"fixture refusal"}),
        )
        .await;
        assert!(
            matches!(call.await.unwrap(), Err(AppOpenError::Refused { code: actual, message }) if actual == code && message == "fixture refusal")
        );
    }
}

#[tokio::test]
async fn connection_operations_preserve_call_failures() {
    let (consumer, _rx) = fake_consumer();
    consumer.close().await;
    assert!(consumer.describe_connection().await.is_err());
    assert!(matches!(
        consumer.app_open("fixture").await,
        Err(AppOpenError::Call(_))
    ));

    let (consumer, mut rx) = fake_consumer();
    let caller = consumer.clone();
    let call = tokio::spawn(async move { caller.app_open("fixture").await });
    answer(
        &consumer,
        rx.recv().await.unwrap(),
        FrameType::Response,
        json!({"op":"wrong", "pid":42, "status":"launched"}),
    )
    .await;
    assert!(matches!(call.await.unwrap(), Err(AppOpenError::Call(_))));
}
