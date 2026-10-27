//! Module questions for MCP hosts on protocol 2026-07-28 or later.
//!
//! That revision forbids server-to-client requests, so the reverse-request
//! relay older hosts get (the gateway sends the host an `elicitation/create`
//! request of its own) is not available. Instead, when a module asks a
//! question while it handles a tools/call, the gateway answers that tools/call
//! with the question in the `inputRequests` form of MCP multi round-trip
//! requests (MRTR) and an opaque `requestState`, and keeps the module's call
//! open: the call is "held". The
//! host asks the person, then repeats the tools/call with `inputResponses` and
//! the same `requestState`. The gateway passes the answer to the module and
//! goes on waiting for the module's result, which answers the repeated call.
//!
//! The module never notices the difference: its reverse request is answered
//! under its own corr, exactly as the relay answers it for older hosts.

use super::*;
use tokio::sync::oneshot;

/// Key under which a module's question appears in `inputRequests`. A held
/// call carries one question at a time, so a fixed key is enough.
pub(super) const MRTR_INPUT_KEY: &str = "elicitation";

/// The answer a module gets when the host never comes back with one before
/// the relay TTL runs out: the same answer a person declining gives.
fn unanswered_elicitation() -> serde_json::Value {
    serde_json::json!({ "action": "decline" })
}

/// A held call's place in its session's list of calls a module question may
/// travel on. Dropping it takes the call off the list and answers any
/// question still in its hands with an Error frame, so the module never
/// waits out its own timeout on a question nobody will carry.
pub(super) struct InputWaiterGuard {
    session: Arc<RelaySession>,
    relay: Arc<ReverseRelay>,
    id: u64,
    rx: mpsc::Receiver<InputAsk>,
    /// The module question that went to the host and has not been answered.
    outstanding: Option<PendingKey>,
}

impl std::fmt::Debug for InputWaiterGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputWaiterGuard")
            .field("id", &self.id)
            .field("outstanding", &self.outstanding)
            .finish_non_exhaustive()
    }
}

impl InputWaiterGuard {
    pub(super) fn register(
        session: Arc<RelaySession>,
        relay: Arc<ReverseRelay>,
        route: RouteHandle,
        corr: u64,
        eligible: bool,
    ) -> Self {
        let (id, rx) = session.register_input_waiter(route, corr, eligible);
        Self {
            session,
            relay,
            id,
            rx,
            outstanding: None,
        }
    }
}

impl Drop for InputWaiterGuard {
    fn drop(&mut self) {
        self.session.remove_input_waiter(self.id);
        self.rx.close();
        let mut stranded = Vec::new();
        while let Ok(ask) = self.rx.try_recv() {
            stranded.push(ask.pending_key);
        }
        stranded.extend(self.outstanding.take());
        if stranded.is_empty() {
            return;
        }
        let relay = Arc::clone(&self.relay);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                for key in stranded {
                    relay
                        .settle_with_error(
                            key,
                            ErrorData::internal_error(
                                "the tool call ended before the MCP host answered this question",
                                None,
                            ),
                        )
                        .await;
                }
            });
        }
    }
}

/// The question side of a held call: present only for a call from a host on
/// 2026-07-28 or later.
#[derive(Debug)]
pub(super) struct HeldInput(Option<InputWaiterGuard>);

impl HeldInput {
    /// The next module question for this call. Never resolves for a call
    /// that cannot carry questions, so waiting on it alongside the module's
    /// frames changes nothing for older hosts.
    pub(super) async fn next(&mut self) -> InputAsk {
        if let Some(guard) = &mut self.0 {
            if let Some(ask) = guard.rx.recv().await {
                return ask;
            }
        }
        std::future::pending().await
    }

    /// Gives the module the host's answer to its outstanding question, or an
    /// Error frame when the host's follow-up carried none. The call is open
    /// to a further question before the answer goes out, because the module
    /// may ask again as soon as it has it.
    async fn deliver_answer(&mut self, answer: Option<&serde_json::Value>) {
        let Some(guard) = &mut self.0 else {
            return;
        };
        let Some(key) = guard.outstanding.take() else {
            return;
        };
        guard.session.clear_input_asked(guard.id);
        match answer {
            Some(answer) => guard.relay.settle_with_answer(key, answer).await,
            None => {
                guard
                    .relay
                    .settle_with_error(
                        key,
                        ErrorData::invalid_params(
                            format!(
                            "the MCP host's follow-up carried no inputResponses.{MRTR_INPUT_KEY}"
                        ),
                            None,
                        ),
                    )
                    .await
            }
        }
    }

    /// Records that `key`'s question went to the host. Returns the deadline
    /// for the host's answer: the reverse-request relay's TTL from when the
    /// module asked.
    fn mark_outstanding(&mut self, ask: &InputAsk) -> time::Instant {
        match &mut self.0 {
            Some(guard) => {
                guard.outstanding = Some(ask.pending_key);
                ask.asked_at + guard.relay.ttl
            }
            None => ask.asked_at + DEFAULT_REVERSE_RELAY_TTL,
        }
    }
}

/// The module's frames for a held call. While the call waits for the host,
/// progress is dropped (there is no host request to report it on) and a
/// terminal frame is kept for the follow-up call.
#[derive(Debug)]
pub(super) struct HeldFrames {
    rx: mpsc::Receiver<SubcFrame>,
    stashed: Option<Option<SubcFrame>>,
}

impl HeldFrames {
    pub(super) async fn next(&mut self) -> Option<SubcFrame> {
        if let Some(stashed) = self.stashed.take() {
            return stashed;
        }
        self.rx.recv().await
    }
}

/// A tools/call sent to a module and not yet answered.
#[derive(Debug)]
pub(super) struct HeldCall {
    pub(super) route: RouteHandle,
    pub(super) corr: u64,
    pub(super) module_id: String,
    /// Whether this attempt was a retry after a module outage; see
    /// `unsent_call_attempt`.
    pub(super) retrying: bool,
    pub(super) frames: HeldFrames,
    pub(super) input: HeldInput,
}

impl HeldCall {
    pub(super) fn new(
        route: RouteHandle,
        corr: u64,
        module_id: String,
        retrying: bool,
        frames: mpsc::Receiver<SubcFrame>,
        input: Option<InputWaiterGuard>,
    ) -> Self {
        Self {
            route,
            corr,
            module_id,
            retrying,
            frames: HeldFrames {
                rx: frames,
                stashed: None,
            },
            input: HeldInput(input),
        }
    }
}

/// What ended one wait on a held call.
pub(super) enum HeldCallEvent {
    Cancelled,
    Frame(Option<SubcFrame>),
    Input(Box<InputAsk>),
}

type ResumeReply = oneshot::Sender<Box<HeldCall>>;

struct ParkedCall {
    /// The tools/call name the host used, which its follow-up has to repeat.
    tool_name: String,
    resume: oneshot::Sender<ResumeReply>,
}

#[derive(Default)]
struct HeldCallsInner {
    next_token: u64,
    parked: HashMap<String, ParkedCall>,
}

/// One MCP session's calls held while their hosts answer a question, keyed by
/// the `requestState` handed to the host. The state is only a handle: a host
/// can resume nothing but its own session's calls, and every token is used
/// once.
#[derive(Clone, Default)]
pub(super) struct HeldCalls {
    inner: Arc<std::sync::Mutex<HeldCallsInner>>,
}

impl HeldCalls {
    fn lock(&self) -> std::sync::MutexGuard<'_, HeldCallsInner> {
        self.inner.lock().unwrap_or_else(|poisoned| {
            tracing::warn!(target: "relay", "recovering from poisoned held-call lock");
            poisoned.into_inner()
        })
    }

    /// Holds `held` until the host's follow-up or the relay TTL, and returns
    /// the answer for the host's tools/call.
    pub(super) fn park(
        &self,
        tool_name: String,
        mut held: Box<HeldCall>,
        ask: InputAsk,
        subc: SubcClient,
    ) -> InputRequiredResult {
        let deadline = held.input.mark_outstanding(&ask);
        let (resume, resumed) = oneshot::channel();
        let token = {
            let mut inner = self.lock();
            inner.next_token += 1;
            let token = format!("subc-mcp-input-{}", inner.next_token);
            inner
                .parked
                .insert(token.clone(), ParkedCall { tool_name, resume });
            token
        };
        tokio::spawn(hold_parked_call(
            held,
            resumed,
            deadline,
            self.clone(),
            token.clone(),
            subc,
        ));
        let mut requests = InputRequests::new();
        requests.insert(MRTR_INPUT_KEY.to_owned(), ask.request);
        InputRequiredResult::new(Some(requests), Some(token))
    }

    /// Takes the call `token` names back for the host's follow-up, gives the
    /// module the host's answer, and returns the call to wait on.
    pub(super) async fn resume(
        &self,
        token: &str,
        tool_name: &str,
        input_responses: Option<&InputResponses>,
    ) -> std::result::Result<Box<HeldCall>, ErrorData> {
        let parked = {
            let mut inner = self.lock();
            match inner.parked.get(token) {
                Some(parked) if parked.tool_name == tool_name => inner.parked.remove(token),
                Some(_) => {
                    return Err(ErrorData::invalid_params(
                        "requestState belongs to a call to a different tool",
                        None,
                    ))
                }
                None => None,
            }
        };
        let Some(parked) = parked else {
            return Err(ErrorData::invalid_params(
                "requestState does not name a tool call waiting for input; it may have expired",
                None,
            ));
        };
        let (reply, replied) = oneshot::channel();
        let ended =
            || ErrorData::internal_error("the tool call ended while it waited for input", None);
        parked.resume.send(reply).map_err(|_| ended())?;
        let mut held = replied.await.map_err(|_| ended())?;
        let answer = input_responses.and_then(|responses| responses.get(MRTR_INPUT_KEY));
        held.input.deliver_answer(answer).await;
        Ok(held)
    }

    /// Takes `token` off the list. False when a follow-up took it first.
    fn claim_for_expiry(&self, token: &str) -> bool {
        self.lock().parked.remove(token).is_some()
    }

    /// Lets go of every held call when the host's session ends. Each one's
    /// module request is cancelled, as an in-flight call's is when its host
    /// goes away.
    pub(super) fn release_all(&self) {
        let parked = std::mem::take(&mut self.lock().parked);
        drop(parked);
    }
}

/// Keeps a parked call until the host's follow-up takes it, the relay TTL
/// runs out, or the session ends.
async fn hold_parked_call(
    mut held: Box<HeldCall>,
    mut resumed: oneshot::Receiver<ResumeReply>,
    deadline: time::Instant,
    calls: HeldCalls,
    token: String,
    subc: SubcClient,
) {
    let expiry = time::sleep_until(deadline);
    tokio::pin!(expiry);
    loop {
        tokio::select! {
            reply = &mut resumed => {
                match reply {
                    Ok(reply) => {
                        let _ = reply.send(held);
                    }
                    // The session ended and dropped the parked entry.
                    Err(_) => cancel_held_call(&held, &subc).await,
                }
                return;
            }
            frame = held.frames.rx.recv(), if held.frames.stashed.is_none() => {
                match frame {
                    Some(frame) if frame.header.ty == FrameType::Push => {}
                    other => held.frames.stashed = Some(other),
                }
            }
            _ = &mut expiry => {
                if calls.claim_for_expiry(&token) {
                    expire_held_call(held, &subc).await;
                    return;
                }
                // A follow-up took the call just as the TTL ran out; hand it over.
                match resumed.await {
                    Ok(reply) => {
                        let _ = reply.send(held);
                    }
                    Err(_) => cancel_held_call(&held, &subc).await,
                }
                return;
            }
        }
    }
}

/// The host never came back with an answer. The module hears what a declined
/// elicitation tells it and finishes its call; nobody is left to read the
/// result, so the gateway stops listening for it.
async fn expire_held_call(mut held: Box<HeldCall>, subc: &SubcClient) {
    if let Some(guard) = &mut held.input.0 {
        if let Some(key) = guard.outstanding.take() {
            guard
                .relay
                .settle_with_answer(key, &unanswered_elicitation())
                .await;
        }
    }
    subc.abandon_request(held.route, held.corr).await;
}

async fn cancel_held_call(held: &HeldCall, subc: &SubcClient) {
    if let Ok(frame) = subc.build_route_frame(
        FrameType::Cancel,
        data_flags(),
        held.route,
        held.corr,
        Vec::new(),
    ) {
        let _ = subc.send_route_frame(held.route, frame).await;
    }
    subc.abandon_request(held.route, held.corr).await;
}

impl ReverseRelay {
    /// Answers a module question with the host's result as the host sent it.
    pub(super) async fn settle_with_answer(&self, key: PendingKey, answer: &serde_json::Value) {
        let handle = self.route_handle(key.0, key.1);
        let frame = match serde_json::to_vec(answer) {
            Ok(body) => self.build_reverse_frame(FrameType::Response, handle, key.2, body),
            Err(error) => self.build_reverse_error(
                handle,
                key.2,
                ErrorData::internal_error(
                    format!("failed to encode the MCP host's answer: {error}"),
                    None,
                ),
            ),
        };
        self.settle_pending_frame(key, frame).await;
    }

    pub(super) async fn settle_with_error(&self, key: PendingKey, error: ErrorData) {
        let frame = self.build_reverse_error(self.route_handle(key.0, key.1), key.2, error);
        self.settle_pending_frame(key, frame).await;
    }
}
