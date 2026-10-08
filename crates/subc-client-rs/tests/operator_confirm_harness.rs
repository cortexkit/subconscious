#![cfg(feature = "test-support")]

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use subc_client_rs::{
    async_trait,
    test_support::{
        BindIdentity, Frame, FrameType, ModuleHarness, OperatorAnswer, OperatorRefusal, RouteTarget,
    },
    HandlerOutcome, ModuleHandle, ModuleHandler, OperatorConfirmError, RequestCtx,
    RouteBindRequest, RouteHandle,
};

const SUMMARY: &str = "Replace identity";

#[derive(Clone, Default)]
struct State {
    writes: Arc<AtomicUsize>,
    answers: Arc<Mutex<Vec<Result<(), OperatorConfirmError>>>>,
}

struct Writer {
    handle: ModuleHandle,
    state: State,
}

#[async_trait]
impl ModuleHandler for Writer {
    async fn handle(&self, ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
        let route = ctx.route_handle();
        let answer = tokio::select! {
            answer = self.handle.confirm_operator(SUMMARY, &route) => answer,
            () = ctx.cancelled() => return HandlerOutcome::Error {
                code: "cancelled".into(), message: "request withdrawn".into(),
            },
        };
        self.state.answers.lock().unwrap().push(answer);
        match answer {
            Ok(()) => {
                self.state.writes.fetch_add(1, Ordering::SeqCst);
                HandlerOutcome::Response(b"written".to_vec())
            }
            Err(error) => HandlerOutcome::Error {
                code: "write_refused".into(),
                message: error.to_string(),
            },
        }
    }
}

async fn start(
    answers: impl IntoIterator<Item = OperatorAnswer>,
) -> (ModuleHarness, RouteHandle, State) {
    let state = State::default();
    let mut module = ModuleHarness::start(
        |handle| Writer {
            handle,
            state: state.clone(),
        },
        answers,
    )
    .await;
    let route = module
        .bind_route(RouteBindRequest::new(
            RouteHandle::detached(7, 3),
            RouteTarget::ToolProvider {
                module_id: "writer".into(),
            },
            BindIdentity::new("/tmp/project", "test", "writer"),
        ))
        .await;
    (module, route, state)
}

async fn write(module: &mut ModuleHarness, route: RouteHandle) -> Frame {
    let corr = module.send_request(route, b"replace".to_vec()).await;
    tokio::time::timeout(Duration::from_secs(2), module.read_reply(route, corr))
        .await
        .expect("handler must reply")
}

fn assert_confirm(module: &ModuleHarness) -> u64 {
    let requests: Vec<_> = module.confirm_requests().collect();
    assert_eq!(requests.len(), 1);
    let (corr, request) = &requests[0];
    assert_eq!(request.summary, SUMMARY);
    assert_eq!((request.route_channel, request.route_epoch), (7, 3));
    let frame = module
        .observed_frames()
        .iter()
        .find(|frame| {
            frame.header.channel == 0
                && frame.header.ty == FrameType::Request
                && frame.header.corr == *corr
        })
        .unwrap();
    assert_eq!(frame.header.epoch, 0);
    *corr
}

fn cancels(module: &ModuleHarness) -> Vec<&Frame> {
    module
        .observed_frames()
        .iter()
        .filter(|frame| frame.header.ty == FrameType::Cancel)
        .collect()
}

#[tokio::test]
async fn scripted_confirmed_allows_write() {
    let (mut module, route, state) = start([OperatorAnswer::Confirmed]).await;
    let reply = write(&mut module, route).await;
    assert_eq!(reply.header.ty, FrameType::Response);
    assert_eq!(reply.body, b"written");
    assert_eq!(state.writes.load(Ordering::SeqCst), 1);
    assert_eq!(*state.answers.lock().unwrap(), [Ok(())]);
    assert_confirm(&module);
    module.shutdown().await;
    assert!(
        cancels(&module).is_empty(),
        "a terminal answer must disarm cancellation"
    );
}

#[tokio::test]
async fn scripted_refusals_prevent_writes() {
    for (refusal, expected) in [
        (OperatorRefusal::Declined, OperatorConfirmError::Declined),
        (
            OperatorRefusal::PresenceUnavailable,
            OperatorConfirmError::PresenceUnavailable,
        ),
        (
            OperatorRefusal::SummaryInvalid,
            OperatorConfirmError::SummaryInvalid,
        ),
        (
            OperatorRefusal::NotPermitted,
            OperatorConfirmError::NotPermitted,
        ),
    ] {
        let (mut module, route, state) = start([OperatorAnswer::Refused {
            refusal,
            reason: "person or policy refused".into(),
        }])
        .await;
        let reply = write(&mut module, route).await;
        assert_eq!(reply.header.ty, FrameType::Error, "{refusal:?}");
        assert_eq!(state.writes.load(Ordering::SeqCst), 0, "{refusal:?}");
        assert_eq!(
            *state.answers.lock().unwrap(),
            [Err(expected)],
            "{refusal:?}"
        );
        assert_confirm(&module);
        module.shutdown().await;
        assert!(
            cancels(&module).is_empty(),
            "a terminal refusal must disarm cancellation"
        );
    }
}

#[tokio::test]
async fn unknown_op_maps_to_unsupported_without_writing() {
    let (mut module, route, state) = start([OperatorAnswer::UnknownOp]).await;
    assert_eq!(write(&mut module, route).await.header.ty, FrameType::Error);
    assert_eq!(state.writes.load(Ordering::SeqCst), 0);
    assert_eq!(
        *state.answers.lock().unwrap(),
        [Err(OperatorConfirmError::Unsupported)]
    );
    assert_confirm(&module);
    module.shutdown().await;
    assert!(cancels(&module).is_empty());
}

#[tokio::test]
async fn dropped_confirmation_mid_handler_wait_sends_one_channel_zero_cancel() {
    let (mut module, route, state) = start([OperatorAnswer::NoAnswer]).await;
    let corr = module.send_request(route, b"replace".to_vec()).await;
    let frame = tokio::time::timeout(Duration::from_secs(2), module.next_frame())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.header.ty, FrameType::Request);
    assert_eq!(frame.header.channel, 0);
    let confirm_corr = assert_confirm(&module);
    module.cancel_request(route, corr).await;
    let reply = tokio::time::timeout(Duration::from_secs(2), module.read_reply(route, corr))
        .await
        .unwrap();
    assert_eq!(reply.header.ty, FrameType::Error);
    let body: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(body["code"], "cancelled");
    assert_eq!(state.writes.load(Ordering::SeqCst), 0);
    assert!(state.answers.lock().unwrap().is_empty());
    // Drain to EOF before counting, so a duplicate queued after the data reply
    // cannot escape the assertion.
    module.shutdown().await;
    let cancels = cancels(&module);
    assert_eq!(cancels.len(), 1);
    assert_eq!(
        (
            cancels[0].header.channel,
            cancels[0].header.epoch,
            cancels[0].header.corr
        ),
        (0, 0, confirm_corr)
    );
    assert!(cancels[0].body.is_empty());
}

#[tokio::test(start_paused = true)]
async fn silent_daemon_reaches_real_deadline_without_writing() {
    let (mut module, route, state) = start([OperatorAnswer::NoAnswer]).await;
    let corr = module.send_request(route, b"replace".to_vec()).await;
    let frame = module.next_frame().await.unwrap();
    assert_eq!(frame.header.ty, FrameType::Request);
    let confirm_corr = assert_confirm(&module);
    tokio::time::advance(Duration::from_secs(289)).await;
    tokio::task::yield_now().await;
    assert!(
        state.answers.lock().unwrap().is_empty(),
        "confirmation timed out early"
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(
        module.read_reply(route, corr).await.header.ty,
        FrameType::Error
    );
    assert_eq!(state.writes.load(Ordering::SeqCst), 0);
    assert_eq!(
        *state.answers.lock().unwrap(),
        [Err(OperatorConfirmError::PresenceUnavailable)]
    );
    module.shutdown().await;
    let cancels = cancels(&module);
    assert_eq!(cancels.len(), 1);
    assert_eq!(
        (
            cancels[0].header.channel,
            cancels[0].header.epoch,
            cancels[0].header.corr
        ),
        (0, 0, confirm_corr)
    );
}

#[tokio::test]
async fn each_confirmation_consumes_its_own_scripted_answer() {
    let (mut module, route, state) = start([
        OperatorAnswer::Confirmed,
        OperatorAnswer::Refused {
            refusal: OperatorRefusal::Declined,
            reason: "only once".into(),
        },
    ])
    .await;
    assert_eq!(
        write(&mut module, route).await.header.ty,
        FrameType::Response
    );
    assert_eq!(write(&mut module, route).await.header.ty, FrameType::Error);
    assert_eq!(state.writes.load(Ordering::SeqCst), 1);
    assert_eq!(
        *state.answers.lock().unwrap(),
        [Ok(()), Err(OperatorConfirmError::Declined)]
    );
    let requests: Vec<_> = module.confirm_requests().collect();
    assert_eq!(requests.len(), 2);
    assert_ne!(requests[0].0, requests[1].0);
    module.shutdown().await;
    assert!(cancels(&module).is_empty());
}
