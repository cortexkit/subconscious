//! Single-use connection claims. Lock order is the spawn's publication/retirement
//! lock, then launch-local claims, then the token/connection index. Index lookups
//! release their lock before acquiring a spawn lock. No lock crosses an await;
//! forwarding closes run after retirement releases the transition lock.

use super::*;
use crate::supervise::RegistrationSpawn;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use subc_control::{
    Claim, ClaimMintReply, ClaimMintRequest, ClaimPresentReply, ClaimPresentRequest, ClaimPurpose,
    ClaimRateLimitedDetail,
};

// Reclaim expired claims before counting: abandoned connection attempts must not
// permanently exhaust a spawn's allowance of outstanding bearer credentials.
const MAX_UNREDEEMED_CLAIMS_PER_SPAWN: usize = 64;
// Bound all purposes together so helpers and relays cannot bypass the spawn's
// live-connection resource budget by choosing a different purpose.
const MAX_LIVE_CLAIM_CONNECTIONS_PER_SPAWN: usize = 256;
// A token bucket bounds sustained mint work while allowing SDK pool refills.
const CLAIM_MINT_RATE_PER_SEC: u128 = 32;
// The initial burst accommodates concurrent SDK pools without an unbounded run.
const CLAIM_MINT_BURST: u128 = 64;
const CLAIM_LIFETIME: Duration = Duration::from_secs(30);
const TOKEN: u128 = 1_000_000_000;

#[derive(Clone)]
pub(crate) struct ClaimBinding {
    launch: Arc<LaunchClaims>,
    pub(crate) incarnation: String,
    // The primary channel's connection id is unique within this incarnation and
    // remains the spawn key even when a candidate is promoted or begins draining.
    pub(crate) spawn: ConnectionId,
    pub(crate) purpose: ClaimPurpose,
    pub(crate) registration: Arc<RegistrationSpawn>,
}

impl fmt::Debug for ClaimBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClaimBinding")
            .field("incarnation", &self.incarnation)
            .field("spawn", &self.spawn)
            .field("purpose", &self.purpose)
            .field("module_id", &self.registration.module_id)
            .finish()
    }
}

struct Pending {
    purpose: ClaimPurpose,
    expires: tokio::time::Instant,
}

struct Bucket {
    tokens: u128,
    updated: tokio::time::Instant,
}

impl Bucket {
    fn take(&mut self, now: tokio::time::Instant) -> Result<(), u64> {
        self.tokens = (self.tokens
            + now.saturating_duration_since(self.updated).as_nanos() * CLAIM_MINT_RATE_PER_SEC)
            .min(CLAIM_MINT_BURST * TOKEN);
        self.updated = now;
        if self.tokens < TOKEN {
            return Err((TOKEN - self.tokens).div_ceil(CLAIM_MINT_RATE_PER_SEC * 1_000_000) as u64);
        }
        self.tokens -= TOKEN;
        Ok(())
    }
}

#[derive(Default)]
struct State {
    pending: HashMap<[u8; 16], std::sync::Weak<LaunchClaims>>,
    bindings: HashMap<ConnectionId, ClaimBinding>,
}

struct LaunchState {
    active: bool,
    pending: HashMap<[u8; 16], Pending>,
    connections: HashMap<ConnectionId, crate::FrameSink>,
    bucket: Bucket,
}

pub(crate) struct LaunchClaims {
    primary: ConnectionId,
    incarnation: String,
    registration: std::sync::Weak<RegistrationSpawn>,
    table: std::sync::Weak<Claims>,
    forwarding: Arc<ForwardingTable>,
    state: Mutex<LaunchState>,
}

impl fmt::Debug for LaunchClaims {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaunchClaims")
            .field("primary", &self.primary)
            .finish_non_exhaustive()
    }
}

impl LaunchClaims {
    // Called under the spawn transition lock. Fence ingress before releasing the
    // lock, then deliver close notifications after releasing the transition,
    // launch-local claims, and token/connection index locks.
    pub(crate) fn revoke(&self) -> Vec<ConnectionId> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.active = false;
        if let Some(table) = self.table.upgrade() {
            let mut index = table.0.lock().unwrap_or_else(|p| p.into_inner());
            for token in state.pending.keys() {
                index.pending.remove(token);
            }
        }
        state.pending.clear();
        let connections = std::mem::take(&mut state.connections);
        for sink in connections.values() {
            sink.terminate_ingress();
        }
        connections.into_keys().collect()
    }

    pub(crate) fn close_revoked(&self, connections: Vec<ConnectionId>) {
        for connection in connections {
            self.forwarding.request_connection_close(
                connection,
                CloseReason::new("claim_revoked", "the supervised launch ended"),
            );
        }
    }
}

#[derive(Default)]
pub(super) struct Claims(
    Mutex<State>,
    #[cfg(test)] std::sync::atomic::AtomicBool,
    #[cfg(test)] Mutex<Vec<u64>>,
    #[cfg(test)] Mutex<Option<Arc<AttachHold>>>,
);

#[cfg(test)]
struct AttachHold {
    reached: tokio::sync::Notify,
    release: std::sync::Barrier,
}

impl Claims {
    #[cfg(test)]
    pub(super) fn record_dispatch(&self, corr: u64) {
        self.2.lock().unwrap().push(corr);
    }
    pub(crate) fn binding(&self, connection: ConnectionId) -> Option<ClaimBinding> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .bindings
            .get(&connection)
            .cloned()
    }

    pub(super) fn connection_closed(&self, connection: ConnectionId) {
        let binding = self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .bindings
            .remove(&connection);
        if let Some(binding) = binding {
            binding
                .launch
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .connections
                .remove(&connection);
        }
    }
}

impl ControlHandler {
    pub(super) fn handle_claim_mint(
        &self,
        connection: ConnectionId,
        frame: &Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        let request = match serde_json::from_slice::<ClaimMintRequest>(&frame.body) {
            Ok(request) => request,
            Err(_) => {
                return Ok(vec![control_error_frame(
                    frame,
                    "invalid_control_body",
                    "invalid claim.mint request",
                )?])
            }
        };
        if self.claims.binding(connection).is_some() {
            return self.claim_refusal(frame, error_codes::CLAIM_MINT_NOT_ALLOWED);
        }
        let Some(spawn) = self.supervisor.registration_spawn(connection) else {
            return self.claim_refusal(frame, error_codes::CLAIM_MINT_NOT_ALLOWED);
        };
        if !spawn.registered()
            || self
                .forwarding
                .module_id_for_connection(connection)
                .ok()
                .flatten()
                .as_deref()
                != Some(spawn.module_id.as_str())
            || spawn.check().is_err()
        {
            return self.claim_refusal(frame, error_codes::CLAIM_MINT_NOT_ALLOWED);
        }
        // Prefix claims authorize a child's HELLO under its owner's configured
        // module-id prefix. Consumer-purpose minting cannot grant that permission
        // without checking ownership of the child's exact requested module id.
        if !matches!(
            request.purpose,
            ClaimPurpose::SelfConnection | ClaimPurpose::Helper | ClaimPurpose::Relay
        ) {
            return self.claim_refusal(frame, error_codes::CLAIM_PREFIX_REFUSED);
        }
        let incarnation = self.supervisor.spawn_snapshot().cursor.daemon_incarnation;
        let Ok(_alive) = spawn.begin_publication() else {
            return self.claim_refusal(frame, error_codes::CLAIM_MINT_NOT_ALLOWED);
        };
        let now = tokio::time::Instant::now();
        let launch = spawn
            .claims
            .get_or_init(|| {
                Arc::new(LaunchClaims {
                    primary: connection,
                    incarnation,
                    registration: Arc::downgrade(&spawn),
                    table: Arc::downgrade(&self.claims),
                    forwarding: self.forwarding.clone(),
                    state: Mutex::new(LaunchState {
                        active: true,
                        pending: HashMap::new(),
                        connections: HashMap::new(),
                        bucket: Bucket {
                            tokens: CLAIM_MINT_BURST * TOKEN,
                            updated: now,
                        },
                    }),
                })
            })
            .clone();
        let mut state = launch.state.lock().unwrap_or_else(|p| p.into_inner());
        let expired = state
            .pending
            .iter()
            .filter(|(_, pending)| pending.expires <= now)
            .map(|(token, _)| *token)
            .collect::<Vec<_>>();
        if !expired.is_empty() {
            let mut index = self.claims.0.lock().unwrap_or_else(|p| p.into_inner());
            for token in expired {
                state.pending.remove(&token);
                index.pending.remove(&token);
            }
        }
        if state.pending.len() >= MAX_UNREDEEMED_CLAIMS_PER_SPAWN {
            return self.claim_refusal(frame, error_codes::CLAIM_UNREDEEMED_LIMIT);
        }
        if let Err(retry_after_ms) = state.bucket.take(now) {
            let error = subc_protocol::ErrorBody {
                code: error_codes::CLAIM_RATE_LIMITED.into(),
                message: "claim mint rate exceeded".into(),
                detail: Some(
                    serde_json::to_value(ClaimRateLimitedDetail::new(retry_after_ms))
                        .expect("claim rate detail serializes"),
                ),
            };
            return Ok(vec![control_error_body_frame(frame, error)?]);
        }
        let mut index = self.claims.0.lock().unwrap_or_else(|p| p.into_inner());
        let mut token = [0; 16];
        loop {
            getrandom::getrandom(&mut token).map_err(|_| {
                RouterError::backend(0, frame.header.corr, "claim entropy unavailable")
            })?;
            if !index.pending.contains_key(&token) {
                break;
            }
        }
        state.pending.insert(
            token,
            Pending {
                purpose: request.purpose,
                expires: now + CLAIM_LIFETIME,
            },
        );
        index.pending.insert(token, Arc::downgrade(&launch));
        let expires_at_ms =
            crate::clock::wall_now_ms().saturating_add(CLAIM_LIFETIME.as_millis() as u64);
        Ok(vec![control_response_body_frame(
            frame,
            &ClaimMintReply::new(Claim::new(URL_SAFE_NO_PAD.encode(token)), expires_at_ms),
            "claim.mint",
        )?])
    }

    pub(super) async fn handle_claim_present(
        &self,
        ctx: &RouteCtx,
        frame: &Frame,
        first: bool,
    ) -> Result<Vec<Frame>, RouterError> {
        let connection = ctx.connection_id;
        let refusal = if !first {
            Some(error_codes::CLAIM_PRESENT_NOT_FIRST)
        } else {
            self.consume_claim(ctx, frame).err()
        };
        if let Some(code) = refusal {
            // Close even on retryable refusal: no failed bearer presentation may
            // leave an authenticated but unbound direct connection behind.
            ctx.egress.terminate_ingress();
            let refusal = control_error_frame(frame, code, "claim presentation refused")?;
            let sent = ctx.egress.send_flushed(refusal).await;
            #[cfg(test)]
            let notify = !self.claims.1.load(std::sync::atomic::Ordering::Relaxed);
            #[cfg(not(test))]
            let notify = true;
            if notify {
                self.forwarding.request_connection_close(
                    connection,
                    CloseReason::new(code, "claim presentation refused"),
                );
            }
            sent?;
            return Ok(Vec::new());
        }
        Ok(vec![control_response_body_frame(
            frame,
            &ClaimPresentReply::new(),
            "claim.present",
        )?])
    }

    fn consume_claim(&self, ctx: &RouteCtx, frame: &Frame) -> Result<(), &'static str> {
        let connection = ctx.connection_id;
        let request = serde_json::from_slice::<ClaimPresentRequest>(&frame.body)
            .map_err(|_| error_codes::CLAIM_INVALID)?;
        let secret = request.claim.expose_secret();
        let mut token = [0; 16];
        if secret.len() != 22 || URL_SAFE_NO_PAD.decode_slice(secret, &mut token) != Ok(16) {
            return Err(error_codes::CLAIM_INVALID);
        }
        let launch = {
            let state = self.claims.0.lock().unwrap_or_else(|p| p.into_inner());
            state
                .pending
                .get(&token)
                .ok_or(error_codes::CLAIM_INVALID)?
                .upgrade()
                .ok_or(error_codes::CLAIM_INVALID)?
        };
        let registration = launch
            .registration
            .upgrade()
            .ok_or(error_codes::CLAIM_INVALID)?;
        let _alive = registration
            .begin_publication()
            .map_err(|_| error_codes::CLAIM_INVALID)?;
        let mut state = launch.state.lock().unwrap_or_else(|p| p.into_inner());
        let mut index = self.claims.0.lock().unwrap_or_else(|p| p.into_inner());
        let pending = state
            .pending
            .remove(&token)
            .ok_or(error_codes::CLAIM_INVALID)?;
        index.pending.remove(&token);
        if pending.expires <= tokio::time::Instant::now()
            || !state.active
            || index.bindings.contains_key(&connection)
        {
            return Err(error_codes::CLAIM_INVALID);
        }
        if state.connections.len() >= MAX_LIVE_CLAIM_CONNECTIONS_PER_SPAWN {
            return Err(error_codes::CLAIM_CONNECTION_LIMIT);
        }
        state.connections.insert(connection, ctx.egress.clone());
        index.bindings.insert(
            connection,
            ClaimBinding {
                launch: launch.clone(),
                incarnation: launch.incarnation.clone(),
                spawn: launch.primary,
                purpose: pending.purpose,
                registration: registration.clone(),
            },
        );
        #[cfg(test)]
        if let Some(hold) = self.claims.3.lock().unwrap().take() {
            hold.reached.notify_one();
            hold.release.wait();
        }
        Ok(())
    }

    fn claim_refusal(&self, frame: &Frame, code: &'static str) -> Result<Vec<Frame>, RouterError> {
        Ok(vec![control_error_frame(
            frame,
            code,
            "claim operation refused",
        )?])
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::supervise::{
        ModuleOverlap, ModuleSpec, RestartPolicy, SupervisedModule, Supervisor,
    };
    use crate::{router::OutboundFrame, Router, RouterConnection};
    use subc_control::ModuleProtocol;
    use subc_protocol::manifest::ModuleManifest;
    use tokio::sync::mpsc;

    struct Net {
        client: tokio::net::TcpStream,
        server: tokio::task::JoinHandle<()>,
    }

    impl Drop for Net {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    impl Net {
        async fn new(router: Arc<Router>) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let info = subc_transport::ConnectionInfo {
                schema: subc_transport::SCHEMA_VERSION,
                wire_version: None,
                endpoints: Vec::new(),
                key: vec![0x42; 32],
                daemon_id: [0x24; 16],
                pid: std::process::id(),
                daemon_ver: "test".into(),
            };
            let auth = crate::server::ServerAuth::new(info.key.clone(), info.daemon_id, "test");
            let server = tokio::spawn(async move {
                crate::server::serve_listener(listener, router, auth)
                    .await
                    .unwrap();
            });
            let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
            subc_transport::authenticate_client(&mut client, &info, Duration::from_secs(2))
                .await
                .unwrap();
            Self { client, server }
        }

        async fn recv(&mut self) -> Option<Frame> {
            tokio::time::timeout(
                Duration::from_secs(2),
                subc_transport::read_frame(&mut self.client),
            )
            .await
            .expect("server must settle the frame")
            .unwrap()
        }

        async fn call(&mut self, frame: Frame) -> Frame {
            subc_transport::write_frame(&mut self.client, &frame)
                .await
                .unwrap();
            self.recv().await.expect("server reply")
        }

        async fn pipeline(&mut self, frames: &[Frame]) {
            use tokio::io::AsyncWriteExt;
            let mut bytes = Vec::new();
            for frame in frames {
                bytes.extend_from_slice(&frame.header.encode());
                bytes.extend_from_slice(&frame.body);
            }
            self.client.write_all(&bytes).await.unwrap();
        }
    }

    #[tokio::test]
    async fn claims_refused_present_fences_pipelined_inline_and_offloaded_requests() {
        let rig = rig().await;
        for (index, (defer_notification, op)) in [
            (true, "catalog.list"),
            (true, "activity.query"),
            (false, "catalog.list"),
            (false, "activity.query"),
        ]
        .into_iter()
        .enumerate()
        {
            // Withholding the close signal models the reader winning the race;
            // delivering it normally also covers the close-first schedule.
            rig.handler
                .claims
                .1
                .store(defer_notification, std::sync::atomic::Ordering::Relaxed);
            let mut net = Net::new(rig.router.clone()).await;
            let bad = request(ClaimPresentRequest::new(Claim::new(
                "AAAAAAAAAAAAAAAAAAAAAA",
            )));
            let corr = 9_000 + index as u64;
            let following = Frame::build(
                FrameType::Request,
                control_flags(),
                0,
                0,
                corr,
                serde_json::to_vec(&serde_json::json!({"op":op})).unwrap(),
            )
            .unwrap();
            net.pipeline(&[bad, following]).await;
            error(&net.recv().await.unwrap(), error_codes::CLAIM_INVALID);
            while let Some(reply) = net.recv().await {
                assert_ne!(
                    reply.header.corr, corr,
                    "a terminal connection cannot answer the pipelined request"
                );
            }
            assert!(
                !rig.handler.claims.2.lock().unwrap().contains(&corr),
                "pipelined request reached control dispatch"
            );
        }
    }

    #[tokio::test]
    async fn claims_bound_connections_close_and_pending_claims_void_on_retirement() {
        let mut rig = rig().await;
        let token = mint(&mut rig, ClaimPurpose::SelfConnection).await.claim;
        let pending = mint(&mut rig, ClaimPurpose::Helper).await.claim;
        let mut net = Net::new(rig.router.clone()).await;
        assert_eq!(
            net.call(request(ClaimPresentRequest::new(token)))
                .await
                .header
                .ty,
            FrameType::Response
        );
        rig.spawn.retire();
        assert!(
            net.recv().await.is_none(),
            "retirement must close the bound server connection"
        );
        assert!(
            rig.handler.claims.0.lock().unwrap().pending.is_empty(),
            "retirement must void pending claims immediately"
        );
        let mut refused = Net::new(rig.router.clone()).await;
        error(
            &refused
                .call(request(ClaimPresentRequest::new(pending)))
                .await,
            error_codes::CLAIM_INVALID,
        );
        assert!(refused.recv().await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn claims_redemption_racing_retirement_is_never_bound_and_alive() {
        let mut rig = rig().await;
        let token = mint(&mut rig, ClaimPurpose::SelfConnection).await.claim;
        let hold = Arc::new(AttachHold {
            reached: tokio::sync::Notify::new(),
            release: std::sync::Barrier::new(2),
        });
        *rig.handler.claims.3.lock().unwrap() = Some(hold.clone());
        let mut net = Net::new(rig.router.clone()).await;
        subc_transport::write_frame(&mut net.client, &request(ClaimPresentRequest::new(token)))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), hold.reached.notified())
            .await
            .expect("redemption attached while holding the transition lock");
        let spawn = rig.spawn.clone();
        let started = Arc::new(tokio::sync::Notify::new());
        let notify = started.clone();
        let retire = tokio::task::spawn_blocking(move || {
            notify.notify_one();
            spawn.retire();
        });
        started.notified().await;
        hold.release.wait();
        retire.await.unwrap();
        while let Some(reply) = net.recv().await {
            assert_eq!(reply.header.ty, FrameType::Response);
        }
        assert!(!rig.spawn.launch_ready());
    }

    struct Wire {
        connection: RouterConnection,
        ctx: RouteCtx,
        replies: mpsc::Receiver<OutboundFrame>,
    }

    impl Wire {
        fn new(router: &Router) -> Self {
            let connection = router.begin_connection();
            let (tx, replies) = mpsc::channel(32);
            let ctx = RouteCtx {
                connection_id: connection.id(),
                egress: crate::FrameSink::new(tx),
            };
            Self {
                connection,
                ctx,
                replies,
            }
        }

        async fn call(&mut self, router: &Arc<Router>, frame: Frame) -> Frame {
            let router = router.clone();
            let ctx = self.ctx.clone();
            let task = tokio::spawn(async move { router.route_for_connection(&ctx, frame).await });
            let mut reply = self.replies.recv().await.expect("router reply");
            if let Some(flushed) = reply.flushed.take() {
                let _ = flushed.send(());
            }
            task.await.unwrap().unwrap();
            reply.frame
        }

        async fn closed(&mut self) {
            let close = self.connection.take_close_receiver();
            tokio::time::timeout(Duration::from_secs(1), close)
                .await
                .expect("refused connection must close")
                .unwrap();
        }
    }

    struct Rig {
        handler: Arc<ControlHandler>,
        router: Arc<Router>,
        primary: Wire,
        spawn: Arc<RegistrationSpawn>,
        _module: SupervisedModule,
    }

    async fn rig() -> Rig {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let handle = SupervisorHandle::new();
        handle.capture_registration_channels_for_test();
        let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
            .with_handle(handle.clone())
            .with_forwarding(forwarding.clone())
            .with_drain_timeout(Duration::ZERO);
        let module = supervisor
            .spawn_async(ModuleSpec {
                module_id: "claims-module".into(),
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "exec sleep 300".into()],
                env: Vec::new(),
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::Subc,
                overlap: ModuleOverlap::Exclusive,
                carry_containment: false,
            })
            .await
            .unwrap();
        let spawn = handle.registration_for_test("claims-module").unwrap();
        spawn.wait_launch().await;
        assert!(spawn.launch_ready());
        let handler =
            Arc::new(ControlHandler::with_forwarding(registry, forwarding).with_supervisor(handle));
        let router = Arc::new(Router::with_control_handler(handler.clone()));
        let mut primary = Wire::new(&router);
        spawn.bind(primary.connection.id());
        let hello = subc_protocol::ModuleHelloBody {
            manifest: ModuleManifest::builder("claims-module", "1.0.0").build(),
            protocol_ver: PROTOCOL_VERSION,
            control_ops: None,
            launch_nonce: None,
        };
        let reply = primary
            .call(
                &router,
                Frame::build(
                    FrameType::Hello,
                    control_flags(),
                    0,
                    0,
                    1,
                    serde_json::to_vec(&hello).unwrap(),
                )
                .unwrap(),
            )
            .await;
        assert_eq!(
            reply.header.ty,
            FrameType::HelloAck,
            "{}",
            String::from_utf8_lossy(&reply.body)
        );
        Rig {
            handler,
            router,
            primary,
            spawn,
            _module: module,
        }
    }

    fn request(body: impl Serialize) -> Frame {
        Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            7,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap()
    }

    async fn mint(rig: &mut Rig, purpose: ClaimPurpose) -> ClaimMintReply {
        let reply = rig
            .primary
            .call(&rig.router, request(ClaimMintRequest::new(purpose)))
            .await;
        assert_eq!(
            reply.header.ty,
            FrameType::Response,
            "{}",
            String::from_utf8_lossy(&reply.body)
        );
        serde_json::from_slice(&reply.body).unwrap()
    }

    async fn present(rig: &Rig, wire: &mut Wire, claim: Claim) -> Frame {
        wire.call(&rig.router, request(ClaimPresentRequest::new(claim)))
            .await
    }

    fn error(reply: &Frame, code: &str) -> ErrorBody {
        assert_eq!(reply.header.ty, FrameType::Error);
        let body: ErrorBody = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(body.code, code);
        body
    }

    #[tokio::test]
    async fn claims_advertised_ops_have_live_handlers() {
        let rig = rig().await;
        for op in ["claim.mint", "claim.present"] {
            assert!(SUBC_CONTROL_OPS.contains(&op));
            assert!(MODULE_TO_SUBC_CONTROL_OPS.contains(&op));
            assert!(subc_ops().iter().any(|entry| entry == op));
        }
        let mut wire = Wire::new(&rig.router);
        let catalog = wire
            .call(
                &rig.router,
                request(serde_json::json!({"op":"catalog.list"})),
            )
            .await;
        let body: serde_json::Value = serde_json::from_slice(&catalog.body).unwrap();
        for op in ["claim.mint", "claim.present"] {
            assert!(body["subc_ops"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry == op));
        }
    }

    #[tokio::test]
    async fn claims_live_primary_mints_all_consumer_purposes_and_fixes_binding() {
        let mut rig = rig().await;
        for purpose in [
            ClaimPurpose::SelfConnection,
            ClaimPurpose::Helper,
            ClaimPurpose::Relay,
        ] {
            let minted = mint(&mut rig, purpose.clone()).await;
            assert_eq!(
                URL_SAFE_NO_PAD
                    .decode(minted.claim.expose_secret())
                    .unwrap()
                    .len(),
                16
            );
            let mut wire = Wire::new(&rig.router);
            let reply = present(&rig, &mut wire, minted.claim).await;
            assert_eq!(reply.header.ty, FrameType::Response);
            let binding = rig.handler.claims.binding(wire.connection.id()).unwrap();
            assert_eq!(binding.purpose, purpose);
            assert_eq!(binding.spawn, rig.primary.connection.id());
            assert_eq!(
                binding.incarnation,
                rig.handler
                    .supervisor
                    .spawn_snapshot()
                    .cursor
                    .daemon_incarnation
            );
            assert!(Arc::ptr_eq(&binding.registration, &rig.spawn));
            let ping = Frame::build(FrameType::Ping, control_flags(), 0, 0, 8, Vec::new()).unwrap();
            assert_eq!(
                wire.call(&rig.router, ping).await.header.ty,
                FrameType::Pong
            );
            assert_eq!(
                rig.handler
                    .claims
                    .binding(wire.connection.id())
                    .unwrap()
                    .purpose,
                purpose
            );
        }
    }

    #[tokio::test]
    async fn claims_mint_requires_live_primary_never_claim_bound_or_nonce() {
        let mut rig = rig().await;
        let mut direct = Wire::new(&rig.router);
        error(
            &direct
                .call(
                    &rig.router,
                    request(ClaimMintRequest::new(ClaimPurpose::SelfConnection)),
                )
                .await,
            error_codes::CLAIM_MINT_NOT_ALLOWED,
        );
        // Even an ordinary registered TCP module cannot mint without a channel.
        let legacy = subc_protocol::ModuleHelloBody {
            manifest: ModuleManifest::builder("legacy-module", "1.0.0").build(),
            protocol_ver: PROTOCOL_VERSION,
            control_ops: None,
            launch_nonce: None,
        };
        let reply = direct
            .call(
                &rig.router,
                Frame::build(
                    FrameType::Hello,
                    control_flags(),
                    0,
                    0,
                    2,
                    serde_json::to_vec(&legacy).unwrap(),
                )
                .unwrap(),
            )
            .await;
        assert_eq!(reply.header.ty, FrameType::HelloAck);
        error(
            &direct
                .call(
                    &rig.router,
                    request(ClaimMintRequest::new(ClaimPurpose::SelfConnection)),
                )
                .await,
            error_codes::CLAIM_MINT_NOT_ALLOWED,
        );
        let token = mint(&mut rig, ClaimPurpose::SelfConnection).await.claim;
        let mut bound = Wire::new(&rig.router);
        assert_eq!(
            present(&rig, &mut bound, token).await.header.ty,
            FrameType::Response
        );
        error(
            &bound
                .call(
                    &rig.router,
                    request(ClaimMintRequest::new(ClaimPurpose::Helper)),
                )
                .await,
            error_codes::CLAIM_MINT_NOT_ALLOWED,
        );
        rig.spawn.retire();
        error(
            &rig.primary
                .call(
                    &rig.router,
                    request(ClaimMintRequest::new(ClaimPurpose::Relay)),
                )
                .await,
            error_codes::CLAIM_MINT_NOT_ALLOWED,
        );
    }

    #[tokio::test]
    async fn claims_goodbye_primary_cannot_mint_after_registration_ends() {
        let mut rig = rig().await;
        let goodbye =
            Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 1, Vec::new()).unwrap();
        rig.router
            .route_for_connection(&rig.primary.ctx, goodbye)
            .await
            .unwrap();
        let reply = rig
            .primary
            .call(
                &rig.router,
                request(ClaimMintRequest::new(ClaimPurpose::SelfConnection)),
            )
            .await;
        error(&reply, error_codes::CLAIM_MINT_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn claims_expire_at_thirty_monotonic_seconds_not_wall_expiry() {
        let mut rig = rig().await;
        tokio::time::pause();
        let early = mint(&mut rig, ClaimPurpose::SelfConnection).await;
        let late = mint(&mut rig, ClaimPurpose::SelfConnection).await;
        tokio::time::advance(Duration::from_millis(29_999)).await;
        let mut wire = Wire::new(&rig.router);
        assert_eq!(
            present(&rig, &mut wire, early.claim).await.header.ty,
            FrameType::Response
        );
        tokio::time::advance(Duration::from_millis(1)).await;
        // Wall time has not advanced thirty seconds: a daemon using this
        // informational timestamp as its authority would still accept the claim.
        assert!(crate::clock::wall_now_ms() < late.expires_at_ms);
        let mut expired = Wire::new(&rig.router);
        error(
            &present(&rig, &mut expired, late.claim).await,
            error_codes::CLAIM_INVALID,
        );
        expired.closed().await;
        assert!(rig
            .handler
            .claims
            .binding(expired.connection.id())
            .is_none());
    }

    #[tokio::test]
    async fn claims_unredeemed_cap_reclaims_expired_before_counting() {
        let mut rig = rig().await;
        tokio::time::pause();
        for _ in 0..64 {
            mint(&mut rig, ClaimPurpose::SelfConnection).await;
        }
        // Refill rate tokens so only the outstanding-claims cap can refuse.
        tokio::time::advance(Duration::from_secs(1)).await;
        let reply = rig
            .primary
            .call(
                &rig.router,
                request(ClaimMintRequest::new(ClaimPurpose::Helper)),
            )
            .await;
        error(&reply, error_codes::CLAIM_UNREDEEMED_LIMIT);
        tokio::time::advance(Duration::from_secs(29)).await;
        let fresh = mint(&mut rig, ClaimPurpose::Relay).await;
        let mut wire = Wire::new(&rig.router);
        assert_eq!(
            present(&rig, &mut wire, fresh.claim).await.header.ty,
            FrameType::Response
        );
        assert_eq!(rig.handler.claims.0.lock().unwrap().pending.len(), 0);
    }

    #[tokio::test]
    async fn claims_live_connection_cap_is_shared_retryable_and_released_on_close() {
        let mut rig = rig().await;
        tokio::time::pause();
        let mut wires = Vec::new();
        for index in 0..256 {
            if index % 32 == 0 {
                tokio::time::advance(Duration::from_secs(1)).await;
            }
            let purpose = match index % 3 {
                0 => ClaimPurpose::SelfConnection,
                1 => ClaimPurpose::Helper,
                _ => ClaimPurpose::Relay,
            };
            let minted = mint(&mut rig, purpose).await;
            let mut wire = Wire::new(&rig.router);
            assert_eq!(
                present(&rig, &mut wire, minted.claim).await.header.ty,
                FrameType::Response
            );
            wires.push(wire);
        }
        let overflow = mint(&mut rig, ClaimPurpose::Helper).await;
        let mut refused = Wire::new(&rig.router);
        let reply = present(&rig, &mut refused, overflow.claim).await;
        let body = error(&reply, error_codes::CLAIM_CONNECTION_LIMIT);
        assert!(error_codes::is_retryable_claim(&body.code));
        refused.closed().await;
        assert!(rig
            .handler
            .claims
            .binding(refused.connection.id())
            .is_none());
        drop(wires.pop());
        let mut replacement = Wire::new(&rig.router);
        let fresh = mint(&mut rig, ClaimPurpose::SelfConnection).await;
        assert_eq!(
            present(&rig, &mut replacement, fresh.claim).await.header.ty,
            FrameType::Response
        );
    }

    #[tokio::test]
    async fn claims_token_bucket_enforces_burst_sustained_rate_and_backoff() {
        let mut rig = rig().await;
        tokio::time::pause();
        for _ in 0..64 {
            let token = mint(&mut rig, ClaimPurpose::SelfConnection).await;
            let mut wire = Wire::new(&rig.router);
            assert_eq!(
                present(&rig, &mut wire, token.claim).await.header.ty,
                FrameType::Response
            );
        }
        for (advance, expected) in [(0, 32), (31, 1)] {
            tokio::time::advance(Duration::from_millis(advance)).await;
            let reply = rig
                .primary
                .call(
                    &rig.router,
                    request(ClaimMintRequest::new(ClaimPurpose::SelfConnection)),
                )
                .await;
            let body = error(&reply, error_codes::CLAIM_RATE_LIMITED);
            assert!(error_codes::is_retryable_claim(&body.code));
            let detail: ClaimRateLimitedDetail =
                serde_json::from_value(body.detail.unwrap()).unwrap();
            assert_eq!(detail.retry_after_ms, expected);
        }
        tokio::time::advance(Duration::from_millis(1)).await;
        mint(&mut rig, ClaimPurpose::SelfConnection).await;
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..32 {
            mint(&mut rig, ClaimPurpose::SelfConnection).await;
        }
        let reply = rig
            .primary
            .call(
                &rig.router,
                request(ClaimMintRequest::new(ClaimPurpose::SelfConnection)),
            )
            .await;
        error(&reply, error_codes::CLAIM_RATE_LIMITED);
    }

    #[tokio::test]
    async fn claims_replay_on_another_connection_is_invalid_and_closed() {
        let mut rig = rig().await;
        let token = mint(&mut rig, ClaimPurpose::SelfConnection).await.claim;
        let mut winner = Wire::new(&rig.router);
        assert_eq!(
            present(&rig, &mut winner, token.clone()).await.header.ty,
            FrameType::Response
        );
        let mut replay = Wire::new(&rig.router);
        error(
            &present(&rig, &mut replay, token).await,
            error_codes::CLAIM_INVALID,
        );
        replay.closed().await;
        assert!(rig.handler.claims.binding(replay.connection.id()).is_none());
        assert!(rig.handler.claims.binding(winner.connection.id()).is_some());
    }

    #[tokio::test]
    async fn claims_second_presentation_closes_without_replacing_binding() {
        let mut rig = rig().await;
        let first = mint(&mut rig, ClaimPurpose::SelfConnection).await.claim;
        let second = mint(&mut rig, ClaimPurpose::Relay).await.claim;
        let mut wire = Wire::new(&rig.router);
        assert_eq!(
            present(&rig, &mut wire, first).await.header.ty,
            FrameType::Response
        );
        error(
            &present(&rig, &mut wire, second).await,
            error_codes::CLAIM_PRESENT_NOT_FIRST,
        );
        wire.closed().await;
        assert_eq!(
            rig.handler
                .claims
                .binding(wire.connection.id())
                .unwrap()
                .purpose,
            ClaimPurpose::SelfConnection
        );
    }

    #[tokio::test]
    async fn claims_data_plane_frame_spends_first_presentation_opportunity() {
        let mut rig = rig().await;
        let token = mint(&mut rig, ClaimPurpose::SelfConnection).await.claim;
        let mut wire = Wire::new(&rig.router);
        let data = Frame::build(
            FrameType::Request,
            control_flags(),
            313,
            0,
            1,
            b"opaque".to_vec(),
        )
        .unwrap();
        let reply = wire.call(&rig.router, data).await;
        error(&reply, "unknown_channel");
        error(
            &present(&rig, &mut wire, token).await,
            error_codes::CLAIM_PRESENT_NOT_FIRST,
        );
        wire.closed().await;
        assert!(rig.handler.claims.binding(wire.connection.id()).is_none());
    }

    #[tokio::test]
    async fn claims_ping_before_present_is_refused_and_closed() {
        let mut rig = rig().await;
        let token = mint(&mut rig, ClaimPurpose::SelfConnection).await.claim;
        let mut wire = Wire::new(&rig.router);
        assert_eq!(
            wire.call(
                &rig.router,
                Frame::build(FrameType::Ping, control_flags(), 0, 0, 1, Vec::new()).unwrap()
            )
            .await
            .header
            .ty,
            FrameType::Pong
        );
        error(
            &present(&rig, &mut wire, token).await,
            error_codes::CLAIM_PRESENT_NOT_FIRST,
        );
        wire.closed().await;
        assert!(rig.handler.claims.binding(wire.connection.id()).is_none());
    }

    #[tokio::test]
    async fn claims_invalid_and_malformed_presentations_never_leave_direct() {
        let rig = rig().await;
        for body in [
            serde_json::json!({"op":"claim.present","claim":"AAAAAAAAAAAAAAAAAAAAAA"}),
            serde_json::json!({"op":"claim.present","claim":"not-base64!"}),
            serde_json::json!({"op":"claim.present"}),
        ] {
            let mut wire = Wire::new(&rig.router);
            error(
                &wire.call(&rig.router, request(body)).await,
                error_codes::CLAIM_INVALID,
            );
            wire.closed().await;
            assert!(rig.handler.claims.binding(wire.connection.id()).is_none());
        }
    }

    #[tokio::test]
    async fn claims_dead_spawn_cannot_redeem_even_an_unexpired_claim() {
        let mut rig = rig().await;
        let token = mint(&mut rig, ClaimPurpose::SelfConnection).await.claim;
        rig.spawn.retire();
        let mut wire = Wire::new(&rig.router);
        error(
            &present(&rig, &mut wire, token).await,
            error_codes::CLAIM_INVALID,
        );
        wire.closed().await;
        assert!(rig.handler.claims.binding(wire.connection.id()).is_none());
    }

    #[tokio::test]
    async fn claims_replacement_cannot_redeem_old_launch_bearer() {
        let mut rig = rig().await;
        let live_token = mint(&mut rig, ClaimPurpose::SelfConnection).await.claim;
        let old_token = mint(&mut rig, ClaimPurpose::Helper).await.claim;
        let mut live = Wire::new(&rig.router);
        assert_eq!(
            present(&rig, &mut live, live_token).await.header.ty,
            FrameType::Response
        );
        let binding = rig.handler.claims.binding(live.connection.id()).unwrap();
        assert!(Arc::ptr_eq(&binding.registration, &rig.spawn));
        assert_eq!(binding.spawn, rig.primary.connection.id());
        rig._module.restart(Some(0)).await.unwrap();
        // The fixture's primary connection is driven by the test, not the
        // sleeping child. End its registration so the restart can finish its
        // drain, while retaining the old bearer to exercise the liveness fence.
        rig.router
            .route_for_connection(
                &rig.primary.ctx,
                Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 2, Vec::new()).unwrap(),
            )
            .await
            .unwrap();
        // Restart acknowledges the command before launching its replacement.
        // Wait for the new launch, not just the old channel's still-ready snapshot.
        let replacement = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let latest = rig
                    .handler
                    .supervisor
                    .registration_for_test("claims-module")
                    .unwrap();
                if !Arc::ptr_eq(&rig.spawn, &latest) {
                    latest.wait_launch().await;
                    break latest;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("restart must create a different registration spawn");
        assert!(replacement.launch_ready());
        assert!(!Arc::ptr_eq(&rig.spawn, &replacement));
        assert!(!rig.spawn.launch_ready());
        let mut primary = Wire::new(&rig.router);
        replacement.bind(primary.connection.id());
        let hello = subc_protocol::ModuleHelloBody {
            manifest: ModuleManifest::builder("claims-module", "1.0.0").build(),
            protocol_ver: PROTOCOL_VERSION,
            control_ops: None,
            launch_nonce: None,
        };
        let reply = primary
            .call(
                &rig.router,
                Frame::build(
                    FrameType::Hello,
                    control_flags(),
                    0,
                    0,
                    1,
                    serde_json::to_vec(&hello).unwrap(),
                )
                .unwrap(),
            )
            .await;
        assert_eq!(
            reply.header.ty,
            FrameType::HelloAck,
            "{}",
            String::from_utf8_lossy(&reply.body)
        );
        let mut fresh = Wire::new(&rig.router);
        error(
            &present(&rig, &mut fresh, old_token).await,
            error_codes::CLAIM_INVALID,
        );
        fresh.closed().await;
        assert!(rig.handler.claims.binding(fresh.connection.id()).is_none());
        assert!(Arc::ptr_eq(
            &rig.handler
                .claims
                .binding(live.connection.id())
                .unwrap()
                .registration,
            &rig.spawn
        ));
    }

    #[tokio::test]
    async fn claims_concurrent_redemption_has_exactly_one_winner() {
        let mut rig = rig().await;
        let token = mint(&mut rig, ClaimPurpose::Helper).await.claim;
        let mut a = Wire::new(&rig.router);
        let mut b = Wire::new(&rig.router);
        let (a_reply, b_reply) = tokio::join!(
            present(&rig, &mut a, token.clone()),
            present(&rig, &mut b, token)
        );
        assert_eq!(
            [a_reply.header.ty, b_reply.header.ty]
                .iter()
                .filter(|ty| **ty == FrameType::Response)
                .count(),
            1
        );
        let (loser, reply) = if a_reply.header.ty == FrameType::Error {
            (&mut a, a_reply)
        } else {
            (&mut b, b_reply)
        };
        error(&reply, error_codes::CLAIM_INVALID);
        loser.closed().await;
        assert!(rig.handler.claims.binding(loser.connection.id()).is_none());
    }

    #[tokio::test]
    async fn claims_reader_order_precedes_offload_in_both_scheduling_orders() {
        let mut rig = rig().await;
        for wait_for_offload in [false, true] {
            let token = mint(&mut rig, ClaimPurpose::SelfConnection).await.claim;
            // This is the server's real post-authentication connection loop,
            // including its activity-task offload and its close/flush handling.
            let (id, mut client, task) =
                crate::server::config_test_connection_loop(rig.router.clone());
            let activity = request(serde_json::json!({"op":"activity.query"}));
            subc_transport::write_frame(&mut client, &activity)
                .await
                .unwrap();
            if wait_for_offload {
                let reply = subc_transport::read_frame(&mut client)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(reply.header.ty, FrameType::Error);
            }
            subc_transport::write_frame(&mut client, &request(ClaimPresentRequest::new(token)))
                .await
                .unwrap();
            let refusal = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let reply = subc_transport::read_frame(&mut client)
                        .await
                        .unwrap()
                        .expect("present refusal before EOF");
                    let body: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
                    if body["code"] == error_codes::CLAIM_PRESENT_NOT_FIRST {
                        break reply;
                    }
                    assert_ne!(
                        reply.header.ty,
                        FrameType::Response,
                        "late present must not win the reader/offload race"
                    );
                }
            })
            .await
            .unwrap();
            error(&refusal, error_codes::CLAIM_PRESENT_NOT_FIRST);
            tokio::time::timeout(Duration::from_secs(2), async {
                while let Some(reply) = subc_transport::read_frame(&mut client).await.unwrap() {
                    assert_ne!(
                        reply.header.ty,
                        FrameType::Response,
                        "a refused claim cannot continue as direct"
                    );
                }
            })
            .await
            .expect("refused presentation must close the real connection");
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(rig.handler.claims.binding(id).is_none());
        }
    }

    #[tokio::test]
    async fn claims_bearer_is_absent_from_daemon_logs() {
        let (output, _capture) = crate::router::test_log::log_capture(tracing::Level::TRACE);
        let mut rig = rig().await;
        let token = mint(&mut rig, ClaimPurpose::SelfConnection).await.claim;
        let mut wire = Wire::new(&rig.router);
        present(&rig, &mut wire, token.clone()).await;
        let mut replay = Wire::new(&rig.router);
        present(&rig, &mut replay, token.clone()).await;
        let logs = crate::router::test_log::captured_logs(&output);
        assert!(
            !logs.is_empty(),
            "the logging assertion must observe daemon events"
        );
        assert!(!logs.contains(token.expose_secret()));
        assert!(!format!("{token:?}").contains(token.expose_secret()));
    }
}
