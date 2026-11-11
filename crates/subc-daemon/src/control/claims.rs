//! Single-use connection claims. Lock order is the spawn's publication/retirement
//! lock, then launch-local claims, then the token/connection index. Index lookups
//! release their lock before acquiring a spawn lock. No lock crosses an await;
//! forwarding closes run after retirement releases the transition lock.
//!
//! GOODBYE prevents the primary from issuing new connection claims and voids
//! pending tokens only. Already-bound consumers belong to the launch, so they
//! remain connected until retirement (observed exit or permanent program refusal).
//!
//! Quota and single-use reservations cover the unlocked content-verification
//! interval. Concurrent attempts cannot overbook a limit or hash the same token
//! twice. The blocking worker retains them through hashing even after caller
//! cancellation; refusal or cancellation then returns them without a binding.

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
    redeeming: Option<Arc<()>>,
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
    primary_live: bool,
    pending: HashMap<[u8; 16], Pending>,
    connections: HashMap<ConnectionId, crate::FrameSink>,
    bucket: Bucket,
    mint_reservations: usize,
    present_reservations: usize,
    #[cfg(test)]
    pending_visits: u64,
    #[cfg(test)]
    index_operations: u64,
}

struct MintPermit {
    launch: Arc<LaunchClaims>,
    reserved: bool,
}

impl MintPermit {
    fn finish(&mut self, state: &mut LaunchState) {
        state.mint_reservations -= 1;
        self.reserved = false;
    }
}

impl Drop for MintPermit {
    fn drop(&mut self) {
        if self.reserved {
            self.launch
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .mint_reservations -= 1;
        }
    }
}

struct PresentPermit {
    launch: Arc<LaunchClaims>,
    registration: Arc<RegistrationSpawn>,
    token: [u8; 16],
    marker: Arc<()>,
    reserved: bool,
}

impl PresentPermit {
    fn finish(&mut self, state: &mut LaunchState) {
        state.present_reservations -= 1;
        self.reserved = false;
    }
}

impl Drop for PresentPermit {
    fn drop(&mut self) {
        if !self.reserved {
            return;
        }
        let mut state = self.launch.state.lock().unwrap_or_else(|p| p.into_inner());
        state.present_reservations -= 1;
        if let Some(pending) = state.pending.get_mut(&self.token) {
            if pending
                .redeeming
                .as_ref()
                .is_some_and(|marker| Arc::ptr_eq(marker, &self.marker))
            {
                pending.redeeming = None;
                if pending.expires <= tokio::time::Instant::now() {
                    state.pending.remove(&self.token);
                    if let Some(table) = self.launch.table.upgrade() {
                        table
                            .0
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .pending
                            .remove(&self.token);
                    }
                }
            }
        }
    }
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
    #[cfg(all(test, unix))]
    pub(crate) fn pending_count_for_test(&self) -> usize {
        self.state.lock().unwrap().pending.len()
    }
    pub(crate) fn end_primary_registration(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.primary_live = false;
        if let Some(table) = self.table.upgrade() {
            let mut index = table.0.lock().unwrap_or_else(|p| p.into_inner());
            for token in state.pending.keys() {
                index.pending.remove(token);
            }
        }
        state.pending.clear();
    }
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
    #[cfg(test)] Mutex<Option<Arc<tokio::sync::Barrier>>>,
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
    #[cfg(all(test, unix))]
    pub(crate) fn expire_claim_for_test(&self, claim: &Claim) {
        let token: [u8; 16] = URL_SAFE_NO_PAD
            .decode(claim.expose_secret())
            .unwrap()
            .try_into()
            .unwrap();
        let launch = self
            .claims
            .0
            .lock()
            .unwrap()
            .pending
            .get(&token)
            .unwrap()
            .upgrade()
            .unwrap();
        launch
            .state
            .lock()
            .unwrap()
            .pending
            .get_mut(&token)
            .unwrap()
            .expires = tokio::time::Instant::now() - Duration::from_secs(1);
    }

    #[cfg(all(test, unix))]
    pub(crate) fn claim_pending_for_test(&self, claim: &Claim) -> bool {
        let token: [u8; 16] = URL_SAFE_NO_PAD
            .decode(claim.expose_secret())
            .unwrap()
            .try_into()
            .unwrap();
        self.claims.0.lock().unwrap().pending.contains_key(&token)
    }

    #[cfg(all(test, unix))]
    pub(crate) fn claim_reserved_for_test(&self, claim: &Claim) -> bool {
        let token: [u8; 16] = URL_SAFE_NO_PAD
            .decode(claim.expose_secret())
            .unwrap()
            .try_into()
            .unwrap();
        let launch = self
            .claims
            .0
            .lock()
            .unwrap()
            .pending
            .get(&token)
            .and_then(std::sync::Weak::upgrade);
        launch.is_some_and(|launch| {
            launch
                .state
                .lock()
                .unwrap()
                .pending
                .get(&token)
                .is_some_and(|pending| pending.redeeming.is_some())
        })
    }

    #[cfg(all(test, unix))]
    pub(crate) fn exhaust_claim_rate_for_test(&self, connection: ConnectionId) {
        let spawn = self.supervisor.registration_spawn(connection).unwrap();
        let mut state = spawn.claims.get().unwrap().state.lock().unwrap();
        state.bucket.tokens = 0;
        state.bucket.updated = tokio::time::Instant::now() + Duration::from_secs(60);
    }
    fn claim_primary_registered(
        &self,
        connection: ConnectionId,
        spawn: &RegistrationSpawn,
    ) -> bool {
        spawn.registered()
            && spawn.launch_ready()
            && self
                .forwarding
                .module_id_for_connection(connection)
                .ok()
                .flatten()
                .as_deref()
                == Some(spawn.module_id.as_str())
    }

    async fn check_claim_program<T: Send + 'static>(
        &self,
        spawn: Arc<RegistrationSpawn>,
        permit: T,
    ) -> Result<(T, bool), tokio::task::JoinError> {
        // Without a program binding, check() only reads launch state and cannot
        // hash executable content.
        if !spawn.has_program_binding() {
            return Ok((permit, spawn.check().is_ok()));
        }
        let dispatcher = tracing::dispatcher::get_default(Clone::clone);
        let span = tracing::Span::current();
        // The worker owns the reservation until hashing ends, even if its caller
        // disconnects and drops the JoinHandle before receiving the result.
        tokio::task::spawn_blocking(move || {
            let allowed = tracing::dispatcher::with_default(&dispatcher, || {
                span.in_scope(|| spawn.check().is_ok())
            });
            (permit, allowed)
        })
        .await
    }
    pub(super) async fn handle_claim_mint(
        &self,
        ctx: &RouteCtx,
        frame: &Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        let connection = ctx.connection_id;
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
        let Some(spawn) = self.supervisor.registration_spawn(connection) else {
            return self.claim_refusal(frame, error_codes::CLAIM_MINT_NOT_ALLOWED);
        };
        if !self.claim_primary_registered(connection, &spawn) {
            return self.claim_refusal(frame, error_codes::CLAIM_MINT_NOT_ALLOWED);
        }
        // Prefix claims register a child module, rather than opening a consumer.
        if !matches!(
            request.purpose,
            ClaimPurpose::SelfConnection | ClaimPurpose::Helper | ClaimPurpose::Relay
        ) {
            return self.claim_refusal(frame, error_codes::CLAIM_PREFIX_REFUSED);
        }
        let permit = {
            let Ok(_alive) = spawn.begin_publication() else {
                return self.claim_refusal(frame, error_codes::CLAIM_MINT_NOT_ALLOWED);
            };
            let now = tokio::time::Instant::now();
            let launch = spawn
                .claims
                .get_or_init(|| {
                    Arc::new(LaunchClaims {
                        primary: connection,
                        incarnation: spawn.incarnation.clone(),
                        registration: Arc::downgrade(&spawn),
                        table: Arc::downgrade(&self.claims),
                        forwarding: self.forwarding.clone(),
                        state: Mutex::new(LaunchState {
                            active: true,
                            primary_live: true,
                            pending: HashMap::new(),
                            connections: HashMap::new(),
                            bucket: Bucket {
                                tokens: CLAIM_MINT_BURST * TOKEN,
                                updated: now,
                            },
                            mint_reservations: 0,
                            present_reservations: 0,
                            #[cfg(test)]
                            pending_visits: 0,
                            #[cfg(test)]
                            index_operations: 0,
                        }),
                    })
                })
                .clone();
            let mut state = launch.state.lock().unwrap_or_else(|p| p.into_inner());
            if !state.active || !state.primary_live {
                return self.claim_refusal(frame, error_codes::CLAIM_MINT_NOT_ALLOWED);
            }
            if let Err(retry_after_ms) = state.bucket.take(now) {
                let error = ErrorBody {
                    code: error_codes::CLAIM_RATE_LIMITED.into(),
                    message: "claim mint rate exceeded".into(),
                    detail: Some(
                        serde_json::to_value(ClaimRateLimitedDetail::new(retry_after_ms))
                            .expect("claim rate detail serializes"),
                    ),
                };
                return Ok(vec![control_error_body_frame(frame, error)?]);
            }
            if state.pending.len() + state.mint_reservations >= MAX_UNREDEEMED_CLAIMS_PER_SPAWN {
                #[cfg(test)]
                {
                    state.pending_visits += state.pending.len() as u64;
                }
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
                        #[cfg(test)]
                        {
                            state.index_operations += 1;
                        }
                    }
                }
                if state.pending.len() + state.mint_reservations >= MAX_UNREDEEMED_CLAIMS_PER_SPAWN
                {
                    return self.claim_refusal(frame, error_codes::CLAIM_UNREDEEMED_LIMIT);
                }
            }
            state.mint_reservations += 1;
            drop(state);
            MintPermit {
                launch,
                reserved: true,
            }
        };
        let (mut permit, verified) = match self.check_claim_program(spawn.clone(), permit).await {
            Ok(result) => result,
            Err(_) => return self.claim_refusal(frame, error_codes::CLAIM_MINT_NOT_ALLOWED),
        };
        if !verified || !self.claim_primary_registered(connection, &spawn) {
            return self.claim_refusal(frame, error_codes::CLAIM_MINT_NOT_ALLOWED);
        }
        if self
            .recovery_request_refusal(ctx, frame, Some("claim.mint"))
            .await?
            .is_some()
        {
            return self.claim_refusal(frame, "config_gated");
        }
        let Ok(_alive) = spawn.begin_publication() else {
            return self.claim_refusal(frame, error_codes::CLAIM_MINT_NOT_ALLOWED);
        };
        let launch = permit.launch.clone();
        let mut state = launch.state.lock().unwrap_or_else(|p| p.into_inner());
        if !state.active || !state.primary_live {
            return self.claim_refusal(frame, error_codes::CLAIM_MINT_NOT_ALLOWED);
        }
        let mut index = self.claims.0.lock().unwrap_or_else(|p| p.into_inner());
        let mut token = [0; 16];
        loop {
            getrandom::getrandom(&mut token).map_err(|_| {
                RouterError::backend(0, frame.header.corr, "claim entropy unavailable")
            })?;
            #[cfg(test)]
            {
                state.index_operations += 1;
            }
            if !index.pending.contains_key(&token) {
                break;
            }
        }
        state.pending.insert(
            token,
            Pending {
                purpose: request.purpose,
                expires: tokio::time::Instant::now() + CLAIM_LIFETIME,
                redeeming: None,
            },
        );
        index.pending.insert(token, Arc::downgrade(&launch));
        #[cfg(test)]
        {
            state.index_operations += 1;
        }
        permit.finish(&mut state);
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
        let refusal = if !first {
            Some(error_codes::CLAIM_PRESENT_NOT_FIRST)
        } else {
            self.consume_claim(ctx, frame).await.err()
        };
        if let Some(code) = refusal {
            return self.refuse_claim_connection(ctx, frame, code).await;
        }
        Ok(vec![control_response_body_frame(
            frame,
            &ClaimPresentReply::new(),
            "claim.present",
        )?])
    }

    pub(super) async fn refuse_claim_connection(
        &self,
        ctx: &RouteCtx,
        frame: &Frame,
        code: &'static str,
    ) -> Result<Vec<Frame>, RouterError> {
        let connection = ctx.connection_id;
        // End ingress before I/O, even for a retryable refusal, so buffered
        // requests cannot run after a rejected claim operation.
        ctx.egress.terminate_ingress();
        let refusal = control_error_frame(frame, code, "claim connection refused")?;
        let sent = ctx.egress.send_flushed(refusal).await;
        #[cfg(test)]
        let notify = !self.claims.1.load(std::sync::atomic::Ordering::Relaxed);
        #[cfg(not(test))]
        let notify = true;
        if notify {
            self.forwarding.request_connection_close(
                connection,
                CloseReason::new(code, "claim connection refused"),
            );
        }
        sent?;
        Ok(Vec::new())
    }

    fn reclaim_expired_claim(&self, launch: &LaunchClaims, token: &[u8; 16]) -> bool {
        let mut state = launch.state.lock().unwrap_or_else(|p| p.into_inner());
        if state
            .pending
            .get(token)
            .is_some_and(|pending| pending.expires <= tokio::time::Instant::now())
        {
            state.pending.remove(token);
            self.claims
                .0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .pending
                .remove(token);
            return true;
        }
        false
    }

    pub(super) fn reclaim_presented_expiry(&self, frame: &Frame) {
        let Ok(request) = serde_json::from_slice::<ClaimPresentRequest>(&frame.body) else {
            return;
        };
        let secret = request.claim.expose_secret();
        let mut token = [0; 16];
        if secret.len() != 22 || URL_SAFE_NO_PAD.decode_slice(secret, &mut token) != Ok(16) {
            return;
        }
        let launch = self
            .claims
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pending
            .get(&token)
            .and_then(std::sync::Weak::upgrade);
        if let Some(launch) = launch {
            self.reclaim_expired_claim(&launch, &token);
        }
    }

    async fn consume_claim(&self, ctx: &RouteCtx, frame: &Frame) -> Result<(), &'static str> {
        let connection = ctx.connection_id;
        let request = serde_json::from_slice::<ClaimPresentRequest>(&frame.body)
            .map_err(|_| error_codes::CLAIM_INVALID)?;
        let secret = request.claim.expose_secret();
        let mut token = [0; 16];
        if secret.len() != 22 || URL_SAFE_NO_PAD.decode_slice(secret, &mut token) != Ok(16) {
            return Err(error_codes::CLAIM_INVALID);
        }
        let launch = {
            let index = self.claims.0.lock().unwrap_or_else(|p| p.into_inner());
            index
                .pending
                .get(&token)
                .and_then(std::sync::Weak::upgrade)
                .ok_or(error_codes::CLAIM_INVALID)?
        };
        #[cfg(test)]
        {
            let barrier = self.claims.4.lock().unwrap().clone();
            if let Some(barrier) = barrier {
                barrier.wait().await;
            }
        }
        if self.reclaim_expired_claim(&launch, &token) {
            return Err(error_codes::CLAIM_INVALID);
        }
        let registration = launch
            .registration
            .upgrade()
            .ok_or(error_codes::CLAIM_INVALID)?;
        if !self.claim_primary_registered(launch.primary, &registration) {
            return Err(error_codes::CLAIM_INVALID);
        }
        let permit = {
            let _alive = registration
                .begin_publication()
                .map_err(|_| error_codes::CLAIM_INVALID)?;
            let mut state = launch.state.lock().unwrap_or_else(|p| p.into_inner());
            let pending = state
                .pending
                .get(&token)
                .ok_or(error_codes::CLAIM_INVALID)?;
            if pending.expires <= tokio::time::Instant::now() {
                state.pending.remove(&token);
                self.claims
                    .0
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .pending
                    .remove(&token);
                return Err(error_codes::CLAIM_INVALID);
            }
            if !state.active || !state.primary_live || pending.redeeming.is_some() {
                return Err(error_codes::CLAIM_INVALID);
            }
            if state.connections.len() + state.present_reservations
                >= MAX_LIVE_CLAIM_CONNECTIONS_PER_SPAWN
            {
                return Err(error_codes::CLAIM_CONNECTION_LIMIT);
            }
            if self
                .claims
                .0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .bindings
                .contains_key(&connection)
            {
                return Err(error_codes::CLAIM_INVALID);
            }
            let marker = Arc::new(());
            state
                .pending
                .get_mut(&token)
                .expect("pending claim checked under the launch lock")
                .redeeming = Some(marker.clone());
            state.present_reservations += 1;
            PresentPermit {
                launch: launch.clone(),
                registration: registration.clone(),
                token,
                marker,
                reserved: true,
            }
        };
        let (mut permit, verified) = self
            .check_claim_program(registration.clone(), permit)
            .await
            .map_err(|_| error_codes::CLAIM_INVALID)?;
        if !verified || !self.claim_primary_registered(launch.primary, &registration) {
            return Err(error_codes::CLAIM_INVALID);
        }
        if self
            .recovery_request_refusal(ctx, frame, Some("claim.present"))
            .await
            .map_err(|_| "config_gated")?
            .is_some()
        {
            return Err("config_gated");
        }
        let _alive = registration
            .begin_publication()
            .map_err(|_| error_codes::CLAIM_INVALID)?;
        let mut state = launch.state.lock().unwrap_or_else(|p| p.into_inner());
        let pending = state
            .pending
            .get(&token)
            .ok_or(error_codes::CLAIM_INVALID)?;
        if pending.expires <= tokio::time::Instant::now()
            || !state.active
            || !state.primary_live
            || !pending
                .redeeming
                .as_ref()
                .is_some_and(|marker| Arc::ptr_eq(marker, &permit.marker))
        {
            return Err(error_codes::CLAIM_INVALID);
        }
        let mut index = self.claims.0.lock().unwrap_or_else(|p| p.into_inner());
        if index.bindings.contains_key(&connection) {
            return Err(error_codes::CLAIM_INVALID);
        }
        let pending = state
            .pending
            .remove(&token)
            .expect("reserved claim checked under the launch lock");
        index.pending.remove(&token);
        state.connections.insert(connection, ctx.egress.clone());
        index.bindings.insert(
            connection,
            ClaimBinding {
                launch: launch.clone(),
                incarnation: launch.incarnation.clone(),
                spawn: launch.primary,
                purpose: pending.purpose,
                registration: permit.registration.clone(),
            },
        );
        permit.finish(&mut state);
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
pub(crate) mod tests {
    use super::*;
    use crate::supervise::{
        ModuleOverlap, ModuleSpec, RestartPolicy, SupervisedModule, Supervisor,
    };
    use crate::{router::OutboundFrame, Router, RouterConnection};
    use subc_control::ModuleProtocol;
    use subc_protocol::manifest::ModuleManifest;
    use tokio::sync::mpsc;

    pub(crate) struct Net {
        client: tokio::net::TcpStream,
        server: tokio::task::JoinHandle<()>,
    }

    impl Drop for Net {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    impl Net {
        pub(crate) fn connection_id(&self, handler: &ControlHandler) -> ConnectionId {
            let peer = self.client.local_addr().unwrap();
            *handler
                .caller_endpoints
                .lock()
                .unwrap()
                .iter()
                .find(|(_, (_, remote))| *remote == peer)
                .unwrap()
                .0
        }
        pub(crate) async fn send(&mut self, frame: Frame) {
            subc_transport::write_frame(&mut self.client, &frame)
                .await
                .unwrap();
        }
        pub(crate) async fn new(router: Arc<Router>) -> Self {
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

        pub(crate) async fn recv(&mut self) -> Option<Frame> {
            tokio::time::timeout(
                Duration::from_secs(2),
                subc_transport::read_frame(&mut self.client),
            )
            .await
            .expect("server must settle the frame")
            .unwrap()
        }

        pub(crate) async fn call(&mut self, frame: Frame) -> Frame {
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
        assert!(
            spawn.launch_ready(),
            "claim fixture launch failed: {:?}",
            module.status()
        );
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

    async fn network_primary(
        rig: &Rig,
        module_id: &str,
    ) -> (Net, Arc<RegistrationSpawn>, SupervisedModule, ConnectionId) {
        network_primary_with_hello(rig, module_id, true).await
    }

    async fn network_primary_with_hello(
        rig: &Rig,
        module_id: &str,
        register: bool,
    ) -> (Net, Arc<RegistrationSpawn>, SupervisedModule, ConnectionId) {
        let handle = rig.handler.supervisor.clone();
        let supervisor =
            Supervisor::new_for_test(rig.handler.registry.clone(), RestartPolicy::default())
                .with_handle(handle.clone())
                .with_forwarding(rig.handler.forwarding.clone());
        let module = supervisor
            .spawn_async(ModuleSpec {
                module_id: module_id.into(),
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "exec sleep 300".into()],
                env: Vec::new(),
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::Subc,
                overlap: ModuleOverlap::Safe,
                carry_containment: false,
            })
            .await
            .unwrap();
        let spawn = handle.registration_for_test(module_id).unwrap();
        spawn.wait_launch().await;
        let mut net = Net::new(rig.router.clone()).await;
        let ping = Frame::build(FrameType::Ping, control_flags(), 0, 0, 2, Vec::new()).unwrap();
        assert_eq!(net.call(ping).await.header.ty, FrameType::Pong);
        let peer = net.client.local_addr().unwrap();
        let connection = *rig
            .handler
            .caller_endpoints
            .lock()
            .unwrap()
            .iter()
            .find(|(_, (_, remote))| *remote == peer)
            .unwrap()
            .0;
        spawn.bind(connection);
        if register {
            let hello = subc_protocol::ModuleHelloBody {
                manifest: ModuleManifest::builder(module_id, "1.0.0").build(),
                protocol_ver: PROTOCOL_VERSION,
                control_ops: None,
                launch_nonce: None,
            };
            let reply = net
                .call(
                    Frame::build(
                        FrameType::Hello,
                        control_flags(),
                        0,
                        0,
                        3,
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
        }
        (net, spawn, module, connection)
    }

    async fn net_mint(net: &mut Net, purpose: ClaimPurpose) -> ClaimMintReply {
        let reply = net.call(request(ClaimMintRequest::new(purpose))).await;
        assert_eq!(
            reply.header.ty,
            FrameType::Response,
            "{}",
            String::from_utf8_lossy(&reply.body)
        );
        serde_json::from_slice(&reply.body).unwrap()
    }

    async fn fill_live_connections(
        rig: &Rig,
        primary: &mut Net,
        spawn: &RegistrationSpawn,
    ) -> Vec<Net> {
        let mut consumers = Vec::new();
        for index in 0..256 {
            if index % 32 == 0 && spawn.claims.get().is_some() {
                spawn
                    .claims
                    .get()
                    .unwrap()
                    .state
                    .lock()
                    .unwrap()
                    .bucket
                    .tokens = CLAIM_MINT_BURST * TOKEN;
            }
            let token = net_mint(primary, ClaimPurpose::SelfConnection).await.claim;
            let mut net = Net::new(rig.router.clone()).await;
            assert_eq!(
                net.call(request(ClaimPresentRequest::new(token)))
                    .await
                    .header
                    .ty,
                FrameType::Response
            );
            consumers.push(net);
        }
        consumers
    }

    #[tokio::test]
    async fn claims_connection_limit_preserves_bearer_for_retry_on_a_new_connection() {
        let rig = rig().await;
        let (mut primary, spawn, _module, _) = network_primary(&rig, "claims-retry").await;
        let mut consumers = fill_live_connections(&rig, &mut primary, &spawn).await;
        let token = net_mint(&mut primary, ClaimPurpose::Helper).await.claim;
        let mut refused = Net::new(rig.router.clone()).await;
        let body = error(
            &refused
                .call(request(ClaimPresentRequest::new(token.clone())))
                .await,
            error_codes::CLAIM_CONNECTION_LIMIT,
        );
        assert!(error_codes::is_retryable_claim(&body.code));
        assert!(refused.recv().await.is_none());
        drop(consumers.pop());
        tokio::time::timeout(Duration::from_secs(2), async {
            while spawn
                .claims
                .get()
                .unwrap()
                .state
                .lock()
                .unwrap()
                .connections
                .len()
                != 255
            {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("closed consumer must release its live-connection slot");
        let mut retry = Net::new(rig.router.clone()).await;
        assert_eq!(
            retry
                .call(request(ClaimPresentRequest::new(token)))
                .await
                .header
                .ty,
            FrameType::Response,
            "retryable connection-cap refusal must not consume the bearer"
        );
    }

    #[tokio::test]
    async fn claims_bound_connections_cannot_hello_register_as_modules() {
        let mut rig = rig().await;
        for (index, purpose) in [
            ClaimPurpose::SelfConnection,
            ClaimPurpose::Helper,
            ClaimPurpose::Relay,
        ]
        .into_iter()
        .enumerate()
        {
            let token = mint(&mut rig, purpose).await.claim;
            let mut consumer = Net::new(rig.router.clone()).await;
            assert_eq!(
                consumer
                    .call(request(ClaimPresentRequest::new(token)))
                    .await
                    .header
                    .ty,
                FrameType::Response
            );
            let module = format!("claimed-{index}");
            let hello = subc_protocol::ModuleHelloBody {
                manifest: ModuleManifest::builder(&module, "1.0.0").build(),
                protocol_ver: PROTOCOL_VERSION,
                control_ops: None,
                launch_nonce: None,
            };
            let reply = consumer
                .call(
                    Frame::build(
                        FrameType::Hello,
                        control_flags(),
                        0,
                        0,
                        8,
                        serde_json::to_vec(&hello).unwrap(),
                    )
                    .unwrap(),
                )
                .await;
            error(&reply, "claim_hello_not_allowed");
            assert!(rig.handler.registry.get_module(&module).unwrap().is_none());
            assert!(consumer.recv().await.is_none());
        }
    }

    #[tokio::test]
    async fn claims_resource_work_is_launch_local_and_rate_admission_is_first() {
        let rig = rig().await;
        let (mut busy, busy_spawn, _busy_module, _) = network_primary(&rig, "claims-busy").await;
        let (mut other, other_spawn, _other_module, _) =
            network_primary(&rig, "claims-other").await;
        net_mint(&mut other, ClaimPurpose::SelfConnection).await;
        let baseline = {
            let state = other_spawn.claims.get().unwrap().state.lock().unwrap();
            (state.pending_visits, state.index_operations)
        };
        assert_eq!(baseline, (0, 2));
        let mut consumers = Vec::new();
        for index in 0..256 {
            if index % 32 == 0 && busy_spawn.claims.get().is_some() {
                busy_spawn
                    .claims
                    .get()
                    .unwrap()
                    .state
                    .lock()
                    .unwrap()
                    .bucket
                    .tokens = CLAIM_MINT_BURST * TOKEN;
            }
            let token = net_mint(&mut busy, ClaimPurpose::SelfConnection)
                .await
                .claim;
            let mut net = Net::new(rig.router.clone()).await;
            assert_eq!(
                net.call(request(ClaimPresentRequest::new(token)))
                    .await
                    .header
                    .ty,
                FrameType::Response
            );
            consumers.push(net);
        }
        busy_spawn
            .claims
            .get()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .bucket
            .tokens = CLAIM_MINT_BURST * TOKEN;
        for _ in 0..64 {
            net_mint(&mut busy, ClaimPurpose::Helper).await;
        }
        {
            let mut state = busy_spawn.claims.get().unwrap().state.lock().unwrap();
            assert_eq!((state.pending.len(), state.connections.len()), (64, 256));
            state.bucket.tokens = 0;
            state.bucket.updated = tokio::time::Instant::now() + Duration::from_secs(60);
            state.pending_visits = 0;
            state.index_operations = 0;
        }
        let refusal = busy
            .call(request(ClaimMintRequest::new(ClaimPurpose::SelfConnection)))
            .await;
        {
            let state = busy_spawn.claims.get().unwrap().state.lock().unwrap();
            assert_eq!(
                (state.pending_visits, state.index_operations),
                (0, 0),
                "rate refusal must precede claim scans and global index work"
            );
        }
        error(&refusal, error_codes::CLAIM_RATE_LIMITED);
        net_mint(&mut other, ClaimPurpose::Relay).await;
        let state = other_spawn.claims.get().unwrap().state.lock().unwrap();
        assert_eq!(
            (state.pending_visits, state.index_operations),
            (baseline.0, baseline.1 + 2),
            "a foreign launch at both caps must add no mint work"
        );
    }

    #[tokio::test]
    async fn claims_primary_goodbye_voids_pending_before_presentation() {
        let rig = rig().await;
        let (mut primary, spawn, _module, _) = network_primary(&rig, "claims-goodbye").await;
        let token = net_mint(&mut primary, ClaimPurpose::SelfConnection)
            .await
            .claim;
        subc_transport::write_frame(
            &mut primary.client,
            &Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 4, Vec::new()).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(
            primary
                .call(Frame::build(FrameType::Ping, control_flags(), 0, 0, 5, Vec::new()).unwrap())
                .await
                .header
                .ty,
            FrameType::Pong
        );
        assert!(
            spawn
                .claims
                .get()
                .unwrap()
                .state
                .lock()
                .unwrap()
                .pending
                .is_empty(),
            "primary deregistration must void pending claims"
        );
        let mut consumer = Net::new(rig.router.clone()).await;
        error(
            &consumer
                .call(request(ClaimPresentRequest::new(token)))
                .await,
            error_codes::CLAIM_INVALID,
        );
        assert!(consumer.recv().await.is_none());
    }

    #[tokio::test]
    async fn claims_goodbye_voids_pending_but_bound_helpers_live_until_retirement() {
        let rig = rig().await;
        let (mut primary, spawn, _module, _) = network_primary(&rig, "claims-goodbye-helper").await;
        let helper = net_mint(&mut primary, ClaimPurpose::Helper).await.claim;
        let pending = net_mint(&mut primary, ClaimPurpose::SelfConnection)
            .await
            .claim;
        let mut consumer = Net::new(rig.router.clone()).await;
        assert_eq!(
            consumer
                .call(request(ClaimPresentRequest::new(helper)))
                .await
                .header
                .ty,
            FrameType::Response
        );
        subc_transport::write_frame(
            &mut primary.client,
            &Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 4, Vec::new()).unwrap(),
        )
        .await
        .unwrap();
        let ping = || Frame::build(FrameType::Ping, control_flags(), 0, 0, 5, Vec::new()).unwrap();
        assert_eq!(primary.call(ping()).await.header.ty, FrameType::Pong);
        assert!(spawn.launch_ready());
        assert!(
            !spawn
                .claims
                .get()
                .unwrap()
                .state
                .lock()
                .unwrap()
                .primary_live
        );
        let mut rejected = Net::new(rig.router.clone()).await;
        error(
            &rejected
                .call(request(ClaimPresentRequest::new(pending)))
                .await,
            error_codes::CLAIM_INVALID,
        );
        assert!(rejected.recv().await.is_none());
        let written = subc_transport::write_frame(&mut consumer.client, &ping()).await;
        let pong = if written.is_ok() {
            consumer.recv().await
        } else {
            None
        };
        assert!(
            matches!(pong, Some(reply) if reply.header.ty == FrameType::Pong),
            "GOODBYE must retain the helper's launch-bound connection"
        );
        let binding = rig
            .handler
            .claims
            .0
            .lock()
            .unwrap()
            .bindings
            .values()
            .find(|binding| binding.spawn != rig.primary.connection.id())
            .cloned()
            .unwrap();
        assert_eq!(binding.purpose, ClaimPurpose::Helper);
        assert!(Arc::ptr_eq(&binding.registration, &spawn));
        spawn.retire();
        assert!(
            consumer.recv().await.is_none(),
            "launch retirement must end the helper connection"
        );
    }

    #[tokio::test]
    async fn claims_present_rechecks_primary_endpoint_even_with_pending_token() {
        let rig = rig().await;
        let (mut primary, spawn, _module, connection) =
            network_primary(&rig, "claims-endpoint").await;
        let token = net_mint(&mut primary, ClaimPurpose::SelfConnection)
            .await
            .claim;
        // Model a stale primary endpoint without using the pending-token cleanup
        // path, so this exercises redemption's independent authority recheck.
        rig.handler
            .forwarding
            .cleanup_connection(connection)
            .unwrap();
        assert!(spawn.launch_ready());
        assert_eq!(
            spawn
                .claims
                .get()
                .unwrap()
                .state
                .lock()
                .unwrap()
                .pending
                .len(),
            1
        );
        let mut consumer = Net::new(rig.router.clone()).await;
        error(
            &consumer
                .call(request(ClaimPresentRequest::new(token)))
                .await,
            error_codes::CLAIM_INVALID,
        );
        assert!(consumer.recv().await.is_none());
    }

    pub(crate) fn request(body: impl Serialize) -> Frame {
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

    pub(crate) fn error(reply: &Frame, code: &str) -> ErrorBody {
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
    async fn claims_direct_connection_cannot_mint() {
        let rig = rig().await;
        let mut direct = Net::new(rig.router.clone()).await;
        error(
            &direct
                .call(request(ClaimMintRequest::new(ClaimPurpose::SelfConnection)))
                .await,
            error_codes::CLAIM_MINT_NOT_ALLOWED,
        );
    }

    #[tokio::test]
    async fn claims_nonce_style_registration_cannot_mint() {
        let rig = rig().await;
        let mut legacy = Net::new(rig.router.clone()).await;
        let hello = subc_protocol::ModuleHelloBody {
            manifest: ModuleManifest::builder("claims-legacy", "1.0.0").build(),
            protocol_ver: PROTOCOL_VERSION,
            control_ops: None,
            launch_nonce: None,
        };
        assert_eq!(
            legacy
                .call(
                    Frame::build(
                        FrameType::Hello,
                        control_flags(),
                        0,
                        0,
                        1,
                        serde_json::to_vec(&hello).unwrap()
                    )
                    .unwrap()
                )
                .await
                .header
                .ty,
            FrameType::HelloAck
        );
        error(
            &legacy
                .call(request(ClaimMintRequest::new(ClaimPurpose::SelfConnection)))
                .await,
            error_codes::CLAIM_MINT_NOT_ALLOWED,
        );
    }

    #[tokio::test]
    async fn claims_bound_connection_cannot_mint() {
        let mut rig = rig().await;
        let token = mint(&mut rig, ClaimPurpose::SelfConnection).await.claim;
        let mut consumer = Net::new(rig.router.clone()).await;
        assert_eq!(
            consumer
                .call(request(ClaimPresentRequest::new(token)))
                .await
                .header
                .ty,
            FrameType::Response
        );
        error(
            &consumer
                .call(request(ClaimMintRequest::new(ClaimPurpose::Helper)))
                .await,
            error_codes::CLAIM_MINT_NOT_ALLOWED,
        );
    }

    #[tokio::test]
    async fn claims_dead_primary_cannot_mint() {
        let rig = rig().await;
        let (mut primary, spawn, _module, _) = network_primary(&rig, "claims-dead").await;
        spawn.retire();
        error(
            &primary
                .call(request(ClaimMintRequest::new(ClaimPurpose::Relay)))
                .await,
            error_codes::CLAIM_MINT_NOT_ALLOWED,
        );
    }

    #[tokio::test]
    async fn claims_primary_must_complete_hello_before_minting() {
        let rig = rig().await;
        let (mut primary, _spawn, _module, _) =
            network_primary_with_hello(&rig, "claims-unregistered", false).await;
        error(
            &primary
                .call(request(ClaimMintRequest::new(ClaimPurpose::SelfConnection)))
                .await,
            error_codes::CLAIM_MINT_NOT_ALLOWED,
        );
    }

    #[tokio::test]
    async fn claims_prefix_authority_is_not_consumer_authority() {
        let rig = rig().await;
        let (mut primary, _spawn, _module, _) = network_primary(&rig, "claims-prefix").await;
        error(
            &primary
                .call(request(ClaimMintRequest::new(ClaimPurpose::Prefix {
                    module_id: "claims-prefix:child".into(),
                })))
                .await,
            error_codes::CLAIM_PREFIX_REFUSED,
        );
    }

    struct SwapRig {
        incumbent: Net,
        candidate: Net,
        module: SupervisedModule,
        swap: tokio::task::JoinHandle<
            Result<crate::supervise::SwapOutcome, crate::supervise::SuperviseError>,
        >,
        incumbent_id: ConnectionId,
        candidate_id: ConnectionId,
    }

    async fn swap_candidate(rig: &Rig, module_id: &str) -> SwapRig {
        let (incumbent, _spawn, module, incumbent_id) = network_primary(rig, module_id).await;
        let swapping = module.clone();
        let swap = tokio::spawn(async move { swapping.swap(Some(Duration::from_secs(5))).await });
        let candidate = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let spawn = rig
                    .handler
                    .supervisor
                    .registration_for_test(module_id)
                    .unwrap();
                if spawn.is_candidate() {
                    spawn.wait_launch().await;
                    break spawn;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("swap must create its candidate channel");
        let mut net = Net::new(rig.router.clone()).await;
        assert_eq!(
            net.call(Frame::build(FrameType::Ping, control_flags(), 0, 0, 2, Vec::new()).unwrap())
                .await
                .header
                .ty,
            FrameType::Pong
        );
        let peer = net.client.local_addr().unwrap();
        let candidate_id = *rig
            .handler
            .caller_endpoints
            .lock()
            .unwrap()
            .iter()
            .find(|(_, (_, remote))| *remote == peer)
            .unwrap()
            .0;
        candidate.bind(candidate_id);
        let mut manifest = ModuleManifest::builder(module_id, "1.0.0").build();
        manifest.ready = Some(false);
        let hello = subc_protocol::ModuleHelloBody {
            manifest,
            protocol_ver: PROTOCOL_VERSION,
            control_ops: None,
            launch_nonce: None,
        };
        let ack = net
            .call(
                Frame::build(
                    FrameType::Hello,
                    control_flags(),
                    0,
                    0,
                    3,
                    serde_json::to_vec(&hello).unwrap(),
                )
                .unwrap(),
            )
            .await;
        assert_eq!(
            ack.header.ty,
            FrameType::HelloAck,
            "{}",
            String::from_utf8_lossy(&ack.body)
        );
        assert_eq!(
            rig.handler
                .forwarding
                .module_id_for_connection(candidate_id)
                .unwrap()
                .as_deref(),
            Some(module_id),
            "candidate must be indexed by its connection"
        );
        SwapRig {
            incumbent,
            candidate: net,
            module,
            swap,
            incumbent_id,
            candidate_id,
        }
    }

    #[tokio::test]
    async fn claims_warming_candidate_primary_can_mint() {
        let rig = rig().await;
        let SwapRig {
            incumbent: _incumbent,
            mut candidate,
            module,
            swap,
            incumbent_id,
            candidate_id,
        } = swap_candidate(&rig, "claims-candidate").await;
        assert_ne!(candidate_id, incumbent_id);
        assert_eq!(
            rig.handler
                .forwarding
                .active_module_endpoint("claims-candidate")
                .unwrap()
                .unwrap()
                .connection_id,
            incumbent_id
        );
        net_mint(&mut candidate, ClaimPurpose::SelfConnection).await;
        drop(candidate);
        drop(_incumbent);
        module.stop().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), swap)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
    }

    #[cfg(feature = "test-support")]
    #[tokio::test]
    async fn claims_draining_incumbent_primary_can_mint() {
        let rig = rig().await;
        let SwapRig {
            mut incumbent,
            mut candidate,
            module: _module,
            swap,
            incumbent_id,
            candidate_id,
        } = swap_candidate(&rig, "claims-draining").await;
        let release = rig.handler.forwarding.hold_route_closing_for_test();
        let ready = candidate
            .call(request(
                serde_json::json!({"op":"catalog.update","provides":[],"ready":true}),
            ))
            .await;
        assert_eq!(ready.header.ty, FrameType::Response);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), swap)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            crate::supervise::SwapOutcome::Cutover
        );
        let notice = incumbent.recv().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&notice.body).unwrap();
        assert_eq!(body["op"], "module.draining");
        assert_eq!(
            rig.handler
                .forwarding
                .active_module_endpoint("claims-draining")
                .unwrap()
                .unwrap()
                .connection_id,
            candidate_id
        );
        assert_eq!(
            rig.handler
                .forwarding
                .module_id_for_connection(incumbent_id)
                .unwrap()
                .as_deref(),
            Some("claims-draining"),
            "draining incumbent must remain indexed by its connection"
        );
        net_mint(&mut incumbent, ClaimPurpose::Relay).await;
        let _ = release.send(());
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn claims_concurrent_redemption_has_exactly_one_winner() {
        let mut rig = rig().await;
        let token = mint(&mut rig, ClaimPurpose::Helper).await.claim;
        let mut a = Net::new(rig.router.clone()).await;
        let mut b = Net::new(rig.router.clone()).await;
        // Both workers look up the same token before either redeems it. The
        // async rendezvous holds no claims lock and never blocks a Tokio worker.
        *rig.handler.claims.4.lock().unwrap() = Some(Arc::new(tokio::sync::Barrier::new(2)));
        let first = request(ClaimPresentRequest::new(token.clone()));
        let second = request(ClaimPresentRequest::new(token));
        let a_task = tokio::spawn(async move {
            let reply = a.call(first).await;
            (a, reply)
        });
        let b_task = tokio::spawn(async move {
            let reply = b.call(second).await;
            (b, reply)
        });
        let ((mut a, a_reply), (mut b, b_reply)) =
            tokio::time::timeout(Duration::from_secs(2), async {
                (a_task.await.unwrap(), b_task.await.unwrap())
            })
            .await
            .expect("two server workers must race redemption");
        *rig.handler.claims.4.lock().unwrap() = None;
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
        assert!(loser.recv().await.is_none());
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
