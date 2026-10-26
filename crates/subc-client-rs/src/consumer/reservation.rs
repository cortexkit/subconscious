use super::*;

/// A library-assigned wire correlation id, unique within its connection.
pub type CorrelationId = u64;

/// A request body validated before a caller-owned database transaction records
/// the send and its correlation id.
#[derive(Debug)]
pub struct RequestBody(Frame);

impl RequestBody {
    /// Validate the body using the same size checks as an ordinary request.
    pub fn new(body: Vec<u8>) -> Result<Self, CallError> {
        Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Interactive, false),
            0,
            0,
            0,
            body,
        )
        .map(Self)
        .map_err(CallError::from)
    }
}

/// Route credit, writer capacity and a correlation id reserved for one request.
///
/// Dropping this value without calling [`Self::send`] sends nothing and releases
/// both the route credit and the writer slot.
#[must_use = "dropping a reservation releases it without sending"]
pub struct RequestReservation {
    writer: mpsc::OwnedPermit<WriteCommand>,
    key: PendingKey,
    reply: PendingReply,
}

impl RequestReservation {
    /// The id already assigned to this request, also returned by [`Self::send`].
    pub fn correlation_id(&self) -> CorrelationId {
        self.key.corr
    }

    /// Synchronously and infallibly hand the request to the connection's writer
    /// queue. No reply polling or socket I/O happens here.
    ///
    /// Queued is not on the wire. If the connection fails after queuing, before
    /// or after writing, `PendingReply` returns the same connection-loss errors
    /// as [`SubcConsumer::request`]. A connection-loss error alone does not tell
    /// you whether the module saw the request; include your own idempotency key
    /// in the body for safe retries when the outcome is uncertain. If the route
    /// closed since reservation, the writer skips the frame and the reply
    /// returns the same closed-route error as an ordinary unwritten request.
    ///
    /// Only `opts.priority` and `opts.admission_class` apply to this already-open
    /// route. The deadline supplied to [`SubcConsumer::reserve`] governs the
    /// entire request, including the reply; `opts.timeout` does not replace it.
    pub fn send(self, body: RequestBody, opts: CallOptions) -> (CorrelationId, PendingReply) {
        let mut frame = body.0;
        frame.header.channel = self.key.channel;
        frame.header.epoch = self.key.epoch;
        frame.header.corr = self.key.corr;
        frame.header.flags =
            Flags::new(false, opts.priority, false).with_admission_class(opts.admission_class);
        self.writer.send(WriteCommand {
            frame,
            pending: Some(self.key),
        });
        (self.key.corr, self.reply)
    }
}

/// The reply to a reserved request, with the same result as
/// [`SubcConsumer::request`]. Dropping it releases the route credit and stops
/// waiting. The writer skips a queued frame if its reply was dropped before
/// dequeue, but dropping the reply cannot recall a frame already dequeued.
#[must_use = "the reply must be awaited to observe the request outcome"]
pub struct PendingReply {
    future: Pin<Box<dyn Future<Output = Result<Vec<u8>, CallError>> + Send>>,
}

impl Future for PendingReply {
    type Output = Result<Vec<u8>, CallError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.future.as_mut().poll(cx)
    }
}

impl SubcConsumer {
    /// Reserve an ordinary request on an already-open route. A caller recording
    /// sends in its own database (for example, SQLite) must await this method
    /// before opening its transaction, so no write lock is held across a wait.
    /// First validate the body with
    /// [`RequestBody::new`], then await `reserve`, then call
    /// [`RequestReservation::send`] inside the transaction without awaiting.
    ///
    /// Checks the route, waits for route credit and writer capacity, and assigns
    /// a correlation id. `deadline` is an absolute Tokio deadline for reservation
    /// and the eventual reply. No frame is queued until `send` is called.
    pub async fn reserve(
        &self,
        route: &RouteHandle,
        deadline: Instant,
    ) -> Result<RequestReservation, CallError> {
        let state = self.shared.route_state(*route)?;
        let permit = match timeout_at(deadline, Arc::clone(&state.sem).acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return Err(CallError::StaleRouteHandle(*route)),
            Err(_) => {
                return Err(CallError::not_sent(
                    "call deadline elapsed waiting for route flow-control",
                ));
            }
        };
        let writer = self
            .shared
            .lock_inner()
            .writer
            .clone()
            .ok_or_else(|| CallError::not_sent("subc connection is down before send"))?;
        let writer = match timeout_at(deadline, writer.reserve_owned()).await {
            Ok(Ok(writer)) => writer,
            Ok(Err(_)) => return Err(CallError::not_sent("writer task closed before reservation")),
            Err(_) => {
                return Err(CallError::not_sent(
                    "call deadline elapsed waiting for writer capacity",
                ));
            }
        };
        if Instant::now() >= deadline {
            return Err(CallError::not_sent(
                "call deadline elapsed before request was sent",
            ));
        }
        let (tx, rx) = oneshot::channel();
        let (key, _) = self
            .shared
            .allocate_request(Some(*route), route.channel, route.epoch)?;
        {
            let mut inner = self.shared.lock_inner();
            if inner.closed || inner.generation != key.generation || inner.writer.is_none() {
                return Err(CallError::not_sent(
                    "connection changed before request registration",
                ));
            }
            if inner.route_epochs.get(&route.channel) != Some(route)
                || inner.retired_routes.contains(route)
            {
                return Err(CallError::StaleRouteHandle(*route));
            }
            // Register before returning so route/connection teardown can settle
            // the reply even when it races the later synchronous queue handoff.
            let mut entry = PendingEntry::unary(tx, false, None, None);
            entry.send_deadline = Some(deadline);
            inner.pending.insert(key, entry);
        }
        let registration = PendingRegistration::new(Arc::clone(&self.shared), key, false);
        let shared = Arc::clone(&self.shared);
        let reply = PendingReply {
            future: Box::pin(async move {
                let _permit = permit;
                match shared.wait_for_reply(registration, rx, deadline).await? {
                    TerminalFrame::Response { body, .. } => Ok(body),
                    TerminalFrame::StreamEnd => Ok(Vec::new()),
                    TerminalFrame::Error { body, .. } => Err(CallError::Module(body)),
                }
            }),
        };
        Ok(RequestReservation { writer, key, reply })
    }
}
