use super::*;
use crate::stand_in::{serve_against_stand_in_with_ops, Served};
use tokio::{sync::Barrier, time::timeout};

struct ReplyHandler {
    barrier: Option<Arc<Barrier>>,
}

#[async_trait]
impl ModuleHandler for ReplyHandler {
    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        if body != b"plain" {
            ctx.clone().mark_binary_reply();
        }
        if let Some(barrier) = &self.barrier {
            barrier.wait().await;
        }
        match body.as_slice() {
            b"error" => HandlerOutcome::Error {
                code: "refused".into(),
                message: "cannot serve bytes".into(),
            },
            b"detail" => HandlerOutcome::ErrorWithDetail {
                code: "refused".into(),
                message: "cannot serve bytes".into(),
                detail: serde_json::json!({"reason": "test"}),
            },
            b"stream" => {
                ctx.emit(vec![0xff, 0]).await.unwrap();
                HandlerOutcome::Streamed
            }
            _ => HandlerOutcome::Response(vec![0xff, 0, 0x80]),
        }
    }
}

async fn start(barrier: Option<Arc<Barrier>>) -> Served {
    let served = serve_against_stand_in_with_ops(|_| ReplyHandler { barrier }, &[]).await;
    served
        .handle
        .install_route(served.handle.route_handle(7, 1))
        .unwrap();
    served
}

async fn request(served: &mut Served, corr: u64, body: &[u8]) {
    let frame = Frame::build(FrameType::Request, data_flags(), 7, 1, corr, body.to_vec()).unwrap();
    write_frame(&mut served.daemon, &frame).await.unwrap();
}

async fn reply(served: &mut Served) -> Frame {
    timeout(Duration::from_secs(2), read_frame(&mut served.daemon))
        .await
        .expect("reply must arrive")
        .unwrap()
        .unwrap()
}

async fn stop(served: Served) {
    served.handle.close_connection();
    assert!(served.serve.await.unwrap().is_ok());
}

#[tokio::test]
async fn binary_reply_mark_on_clone_sets_only_response_binary_flag() {
    let mut served = start(None).await;
    request(&mut served, 1, b"binary").await;
    let response = reply(&mut served).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(
        response.header.flags,
        Flags::new(true, Priority::Interactive, false)
    );
    assert_eq!(response.body, [0xff, 0, 0x80]);
    stop(served).await;
}

#[tokio::test]
async fn binary_reply_mark_does_not_leak_to_next_request_on_same_route() {
    let mut served = start(None).await;
    request(&mut served, 1, b"binary").await;
    assert!(reply(&mut served).await.header.flags.is_binary());
    request(&mut served, 2, b"plain").await;
    let response = reply(&mut served).await;
    assert_eq!(response.header.corr, 2);
    assert_eq!(response.header.flags, data_flags());
    assert_eq!(response.body, [0xff, 0, 0x80]);
    stop(served).await;
}

#[tokio::test]
async fn binary_reply_mark_is_isolated_between_concurrent_requests() {
    let mut served = start(Some(Arc::new(Barrier::new(2)))).await;
    request(&mut served, 1, b"binary").await;
    request(&mut served, 2, b"plain").await;
    let mut responses = [reply(&mut served).await, reply(&mut served).await];
    responses.sort_by_key(|frame| frame.header.corr);
    assert_eq!(responses[0].header.corr, 1);
    assert_eq!(
        responses[0].header.flags,
        Flags::new(true, Priority::Interactive, false)
    );
    assert_eq!(responses[1].header.corr, 2);
    assert_eq!(responses[1].header.flags, data_flags());
    stop(served).await;
}

#[tokio::test]
async fn binary_reply_mark_does_not_change_errors_or_stream_flags() {
    let mut served = start(None).await;
    for (corr, body) in [(1, b"error".as_slice()), (2, b"detail".as_slice())] {
        request(&mut served, corr, body).await;
        let response = reply(&mut served).await;
        assert_eq!(response.header.ty, FrameType::Error);
        assert_eq!(response.header.flags, data_flags());
        let error: ErrorBody = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(error.code, "refused");
        assert_eq!(error.message, "cannot serve bytes");
        assert_eq!(
            error.detail,
            (corr == 2).then(|| serde_json::json!({"reason": "test"}))
        );
    }
    request(&mut served, 3, b"stream").await;
    let data = reply(&mut served).await;
    assert_eq!(data.header.ty, FrameType::StreamData);
    assert_eq!(data.header.flags, data_flags());
    assert_eq!(data.body, [0xff, 0]);
    let end = reply(&mut served).await;
    assert_eq!(end.header.ty, FrameType::StreamEnd);
    assert_eq!(end.header.flags, data_flags());
    assert!(end.body.is_empty());
    stop(served).await;
}
