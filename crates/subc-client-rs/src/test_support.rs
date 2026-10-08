//! Tests-only support for running a module's real serve handler without a daemon.
//!
//! Enable `test-support` **only on a dev-dependency**, never on a production
//! dependency. Cargo features are additive: enabling it on a normal dependency
//! would include this harness in the production build. Without the feature this
//! public module is not compiled.
//!
//! ```toml
//! [dev-dependencies]
//! subc_client = { package = "subc-client-rs", version = "0.26.3", features = ["test-support"] }
//! ```
//!
//! The shared SDK stand-in uses an in-memory framed connection and the same
//! dispatch, control-reply demultiplexer and writer lifecycle as `serve`. It
//! starts after registration; there is no TCP listener, connection file, launch
//! nonce or environment access. It is not a model of daemon policy or presence.
//! Scripted answers prove how the module handles those answers, not whether a
//! real daemon would grant a request.
//!
//! # Approve and refuse a write through the real handler
//!
//! ```
//! # use subc_client_rs as subc_client;
//! use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
//! use subc_client::{async_trait, HandlerOutcome, ModuleHandle, ModuleHandler,
//!     RequestCtx, RouteBindRequest, RouteHandle};
//! use subc_client::test_support::{BindIdentity, FrameType, ModuleHarness,
//!     OperatorAnswer, OperatorRefusal, RouteTarget};
//!
//! struct Writer { handle: ModuleHandle, writes: Arc<AtomicUsize> }
//! #[async_trait]
//! impl ModuleHandler for Writer {
//!     async fn handle(&self, ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
//!         if self.handle.confirm_operator("Replace identity", &ctx.route_handle()).await.is_err() {
//!             return HandlerOutcome::Error {
//!                 code: "write_refused".into(), message: "not confirmed".into(),
//!             };
//!         }
//!         self.writes.fetch_add(1, Ordering::SeqCst);
//!         HandlerOutcome::Response(b"written".to_vec())
//!     }
//! }
//!
//! # tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
//! for (answer, expected_writes) in [
//!     (OperatorAnswer::Confirmed, 1),
//!     (OperatorAnswer::Refused {
//!         refusal: OperatorRefusal::Declined, reason: "person said no".into(),
//!     }, 0),
//! ] {
//!     let writes = Arc::new(AtomicUsize::new(0));
//!     let mut module = ModuleHarness::start(
//!         |handle| Writer { handle, writes: writes.clone() }, [answer],
//!     ).await;
//!     let route = module.bind_route(RouteBindRequest::new(
//!         RouteHandle::detached(7, 3),
//!         RouteTarget::ToolProvider { module_id: "writer".into() },
//!         BindIdentity::new("/tmp/project", "test", "writer"),
//!     )).await;
//!     let corr = module.send_request(route, b"replace".to_vec()).await;
//!     let reply = module.read_reply(route, corr).await;
//!     assert_eq!(reply.header.ty, if expected_writes == 1 { FrameType::Response } else { FrameType::Error });
//!     assert_eq!(writes.load(Ordering::SeqCst), expected_writes);
//!     let (_, request) = module.confirm_requests().next().unwrap();
//!     assert_eq!(request.summary, "Replace identity");
//!     assert_eq!((request.route_channel, request.route_epoch), (7, 3));
//!     module.shutdown().await;
//! }
//! # });
//! ```

use std::collections::VecDeque;

pub use subc_protocol::{
    session::OperatorConfirmRequest, BindIdentity, Frame, FrameType, RouteTarget,
};

use super::{stand_in, *};

/// One scripted daemon answer to one `operator.confirm`, in receive order.
/// Exhausting the script is a test failure, never an implicit approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperatorAnswer {
    /// Return the well-formed `confirmed` response.
    Confirmed,
    /// Return an error with the refusal code and the supplied `detail.reason`.
    Refused {
        refusal: OperatorRefusal,
        reason: String,
    },
    /// Return `unsupported_control_frame`, as a daemon without this op would.
    UnknownOp,
    /// Receive the request but never reply. To test the normal 290-second
    /// deadline quickly, enable Tokio's dev-only `test-util` feature and use a
    /// paused clock (`#[tokio::test(start_paused = true)]` / `time::advance`).
    /// The harness does not change the production helper's timeout.
    NoAnswer,
}

/// The four daemon refusal codes understood by operator confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorRefusal {
    Declined,
    PresenceUnavailable,
    SummaryInvalid,
    NotPermitted,
}

impl OperatorRefusal {
    /// The exact wire error code used by this refusal.
    pub fn code(self) -> &'static str {
        match self {
            Self::Declined => error_codes::OPERATOR_DECLINED,
            Self::PresenceUnavailable => error_codes::OPERATOR_PRESENCE_UNAVAILABLE,
            Self::SummaryInvalid => error_codes::OPERATOR_SUMMARY_INVALID,
            Self::NotPermitted => error_codes::OPERATOR_REQUEST_NOT_PERMITTED,
        }
    }
}

impl OperatorAnswer {
    fn frame(self, corr: u64) -> Option<Frame> {
        let (ty, body) = match self {
            Self::Confirmed => (
                FrameType::Response,
                serde_json::to_vec(&OperatorConfirmReply::confirmed()).unwrap(),
            ),
            Self::Refused { refusal, reason } => (
                FrameType::Error,
                serde_json::to_vec(
                    &ErrorBody::new(refusal.code(), "operator confirmation refused")
                        .with_detail(serde_json::json!({"reason": reason})),
                )
                .unwrap(),
            ),
            Self::UnknownOp => (
                FrameType::Error,
                serde_json::to_vec(&ErrorBody::new(
                    "unsupported_control_frame",
                    "unknown module control operation: operator.confirm",
                ))
                .unwrap(),
            ),
            Self::NoAnswer => return None,
        };
        Some(Frame::build(ty, control_flags(), 0, 0, corr, body).unwrap())
    }
}

/// A module's real serve loop attached to the shared in-process stand-in.
///
/// Reading with [`Self::next_frame`] or [`Self::read_reply`] drives the scripted
/// daemon: each received confirmation consumes one answer. Other module traffic
/// is left untouched. All received frames, including confirms, pushes and
/// channel-0 Cancels, remain available in [`Self::observed_frames`].
///
/// Helpers panic on harness/transport errors or a rejected bind, making those
/// test failures. Module request errors are returned as ordinary Error frames.
/// Put a test timeout around a handler that might never reply. Call
/// [`Self::shutdown`] to await orderly cleanup; dropping the harness also closes
/// the connection, but does not wait for cleanup.
pub struct ModuleHarness {
    served: stand_in::Served,
    answers: VecDeque<OperatorAnswer>,
    observed: Vec<Frame>,
    next_corr: u64,
}

impl ModuleHarness {
    /// Start serving, advertising `operator.confirm` in the synthetic HELLO_ACK.
    /// The factory receives the live handle so the real handler can retain it.
    /// The handler's `on_hello_ack` hook runs before this returns.
    pub async fn start<H: ModuleHandler>(
        make_handler: impl FnOnce(ModuleHandle) -> H,
        answers: impl IntoIterator<Item = OperatorAnswer>,
    ) -> Self {
        Self {
            served: stand_in::serve_against_stand_in_with_ops(make_handler, &[OPERATOR_CONFIRM_OP])
                .await,
            answers: answers.into_iter().collect(),
            observed: Vec::new(),
            next_corr: HELLO_CORR + 1,
        }
    }

    /// Bind through the real `on_bind`/`on_bound` path. Use a detached handle in
    /// `request`; its channel and epoch are kept, but connection identity is
    /// replaced with this harness's identity. All other bind fields are kept.
    /// A rejected bind panics and its error remains in `observed_frames`.
    pub async fn bind_route(&mut self, request: RouteBindRequest) -> RouteHandle {
        assert_ne!(request.handle.channel, 0, "route channel must be nonzero");
        let route = self
            .served
            .handle
            .route_handle(request.handle.channel, request.handle.epoch);
        let body = ModuleControlRequest::RouteBind {
            route_channel: route.channel,
            epoch: route.epoch,
            target: request.target,
            identity: request.identity,
            principal: request.principal,
            consumer_capabilities: request.consumer_capabilities,
            role_versions: request.role_versions,
            admission_facts: request.admission_facts,
            scope: request.scope,
        };
        let corr = self.allocate_corr();
        self.send(
            Frame::build(
                FrameType::Request,
                control_flags(),
                0,
                0,
                corr,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap(),
        )
        .await;
        let reply = self.read_terminal(0, 0, corr).await;
        assert_eq!(
            reply.header.ty,
            FrameType::Response,
            "route.bind refused: {reply:?}"
        );
        route
    }

    /// Send a client-like data request on a bound route and return its correlation.
    pub async fn send_request(&mut self, route: RouteHandle, body: Vec<u8>) -> u64 {
        self.assert_own_route(route);
        let corr = self.allocate_corr();
        self.send(
            Frame::build(
                FrameType::Request,
                data_flags(),
                route.channel,
                route.epoch,
                corr,
                body,
            )
            .unwrap(),
        )
        .await;
        corr
    }

    /// Read this request's Response, Error or StreamEnd, driving daemon answers
    /// along the way. Interim stream frames and other replies stay in the log.
    pub async fn read_reply(&mut self, route: RouteHandle, corr: u64) -> Frame {
        self.assert_own_route(route);
        self.read_terminal(route.channel, route.epoch, corr).await
    }

    /// Send a client-like Cancel. The real handler must honour its context's
    /// cancellation token and drop the confirmation future to withdraw the prompt.
    pub async fn cancel_request(&mut self, route: RouteHandle, corr: u64) {
        self.assert_own_route(route);
        self.send(
            Frame::build(
                FrameType::Cancel,
                data_flags(),
                route.channel,
                route.epoch,
                corr,
                Vec::new(),
            )
            .unwrap(),
        )
        .await;
    }

    /// Read and record one module frame, scripting its answer if it is a confirm.
    /// Returns `None` at EOF. Useful for synchronising a cancellation or clock
    /// advance with a confirmation that has actually reached the stand-in.
    pub async fn next_frame(&mut self) -> Option<Frame> {
        let frame = read_frame(&mut self.served.daemon).await.unwrap()?;
        self.observed.push(frame.clone());
        if frame.header.channel == 0
            && frame.header.ty == FrameType::Request
            && serde_json::from_slice::<OperatorConfirmRequest>(&frame.body).is_ok()
        {
            let answer = self
                .answers
                .pop_front()
                .expect("operator.confirm script exhausted");
            if let Some(reply) = answer.frame(frame.header.corr) {
                self.send(reply).await;
            }
        }
        Some(frame)
    }

    /// Every frame read from the module, in receive order. Cancel frames retain
    /// their exact channel, epoch and correlation id; nothing is synthesised.
    pub fn observed_frames(&self) -> &[Frame] {
        &self.observed
    }

    /// The actual decoded confirm requests with their wire correlation ids.
    pub fn confirm_requests(&self) -> impl Iterator<Item = (u64, OperatorConfirmRequest)> + '_ {
        self.observed.iter().filter_map(|frame| {
            if frame.header.channel == 0 && frame.header.ty == FrameType::Request {
                serde_json::from_slice(&frame.body)
                    .ok()
                    .map(|request| (frame.header.corr, request))
            } else {
                None
            }
        })
    }

    /// Send GOODBYE, drain remaining module traffic into the log, and await the
    /// real serve future (including its bounded writer drain). Call only once.
    pub async fn shutdown(&mut self) {
        self.send(Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 0, Vec::new()).unwrap())
            .await;
        while self.next_frame().await.is_some() {}
        (&mut self.served.serve).await.unwrap().unwrap();
    }

    fn allocate_corr(&mut self) -> u64 {
        let corr = self.next_corr;
        self.next_corr = corr.checked_add(1).expect("test correlation exhausted");
        corr
    }

    fn assert_own_route(&self, route: RouteHandle) {
        assert_eq!(
            route.connection_token(),
            self.served.handle.shared.connection_token,
            "route belongs to another connection"
        );
    }

    async fn send(&mut self, frame: Frame) {
        write_frame(&mut self.served.daemon, &frame).await.unwrap();
        self.served.daemon.flush().await.unwrap();
    }

    async fn read_terminal(&mut self, channel: u16, epoch: u32, corr: u64) -> Frame {
        let matches = |frame: &Frame| {
            frame.header.channel == channel
                && frame.header.epoch == epoch
                && frame.header.corr == corr
                && matches!(
                    frame.header.ty,
                    FrameType::Response | FrameType::Error | FrameType::StreamEnd
                )
        };
        if let Some(frame) = self.observed.iter().find(|frame| matches(frame)) {
            return frame.clone();
        }
        loop {
            let frame = self
                .next_frame()
                .await
                .expect("connection ended before reply");
            if matches(&frame) {
                return frame;
            }
        }
    }
}

impl Drop for ModuleHarness {
    fn drop(&mut self) {
        self.served.handle.close_connection();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_refusals_keep_the_exact_code_and_detail_reason() {
        for (refusal, code) in [
            (OperatorRefusal::Declined, "operator_declined"),
            (
                OperatorRefusal::PresenceUnavailable,
                "operator_presence_unavailable",
            ),
            (OperatorRefusal::SummaryInvalid, "operator_summary_invalid"),
            (
                OperatorRefusal::NotPermitted,
                "operator_request_not_permitted",
            ),
        ] {
            let frame = OperatorAnswer::Refused {
                refusal,
                reason: "audit reason".into(),
            }
            .frame(42)
            .unwrap();
            assert_eq!(frame.header.ty, FrameType::Error);
            assert_eq!(
                (frame.header.channel, frame.header.epoch, frame.header.corr),
                (0, 0, 42)
            );
            let body: serde_json::Value = serde_json::from_slice(&frame.body).unwrap();
            assert_eq!(body["code"], code);
            assert_eq!(body["detail"]["reason"], "audit reason");
        }
    }
}
