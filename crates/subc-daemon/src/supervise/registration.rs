//! Per-launch registration, tracked separately from nonce checks keyed by module ID.
//! Locks only protect snapshots; neither service nor launch waits hold them.

#[cfg(test)]
#[path = "registration_read_tests.rs"]
mod read_tests;
#[cfg(all(test, target_os = "linux"))]
#[path = "retirement_tests.rs"]
mod retirement_tests;
#[cfg(all(test, unix))]
#[path = "registration_review_tests.rs"]
mod review_tests;

use super::*;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Weak,
};

/// The refusal code for any registration operation that reaches a spawn whose
/// exit has been observed. Terminal: the channel never carries authority again.
pub(crate) const REGISTRATION_CHANNEL_CLOSED: &str = "registration_channel_closed";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchState {
    Pending,
    Ready,
    Dead,
}

#[cfg(test)]
#[derive(Debug)]
struct RetirementPause {
    reached: tokio::sync::oneshot::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[derive(Debug)]
pub(crate) struct RegistrationSpawn {
    pub(crate) incarnation: String,
    pub(super) binding_key: program_binding::ProgramBindingKey,
    pub(crate) claims: std::sync::OnceLock<Arc<crate::control::claims::LaunchClaims>>,
    pub(crate) module_id: String,
    /// This launch's authority record, created with it. It dies in the same
    /// retirement step that ends the channel and voids the claims.
    pub(crate) authority: Arc<crate::spawn_authority::SpawnAuthorityRecord>,
    pub(super) role: SpawnRole,
    hello_seen: AtomicBool,
    registered: AtomicBool,
    nonce_registered: AtomicBool,
    deadline: std::sync::OnceLock<Instant>,
    launch: tokio::sync::watch::Sender<LaunchState>,
    transition: Mutex<()>,
    primary_sink: Mutex<Option<crate::FrameSink>>,
    #[cfg(test)]
    retirement_pause: Mutex<Option<RetirementPause>>,
    #[cfg(unix)]
    endpoint: Mutex<Option<std::os::unix::net::UnixStream>>,
    #[cfg(windows)]
    endpoint: Mutex<Option<subc_os::registration_channel::RegistrationPipeShutdown>>,
    binding: Option<ProgramBindingPublication>,
    requires_binding: bool,
    pub(super) identity: std::sync::OnceLock<ProcessIdentity>,
    connections: Weak<Mutex<HashMap<ConnectionId, Arc<RegistrationSpawn>>>>,
}

impl RegistrationSpawn {
    #[cfg(all(test, unix))]
    pub(crate) fn set_content_probe_for_test(&self, probe: content_binding::ContentProbe) {
        self.binding.as_ref().unwrap().set_content_probe(probe);
    }
    #[cfg(all(test, unix))]
    pub(crate) fn defer_content_kill_for_test(&self) -> impl Drop {
        self.binding.as_ref().unwrap().defer_content_kill_for_test()
    }

    #[cfg(all(test, unix))]
    pub(crate) fn content_was_refused_for_test(&self) -> bool {
        self.binding.as_ref().unwrap().content_was_refused()
    }
    #[cfg(test)]
    pub(super) fn new(
        handle: &SupervisorHandle,
        module: &str,
        role: SpawnRole,
        binding: Option<ProgramBindingPublication>,
    ) -> Arc<Self> {
        let required = binding.is_some();
        Self::new_with_binding_requirement(handle, module, role, binding, required)
    }

    pub(super) fn new_with_binding_requirement(
        handle: &SupervisorHandle,
        module: &str,
        role: SpawnRole,
        binding: Option<ProgramBindingPublication>,
        requires_binding: bool,
    ) -> Arc<Self> {
        let (launch, _) = tokio::sync::watch::channel(LaunchState::Pending);
        let binding_key = binding
            .as_ref()
            .map(|binding| binding.channel_key())
            .unwrap_or_else(|| program_binding::ProgramBindingKey::new(handle));
        // The spawn generation names this launch from now on, so a candidate
        // that registers and opens routes before cutover attests the same
        // number it keeps after promotion.
        let (incarnation, generation) = handle.spawn_events.allocate_generation(module);
        let spawn = Arc::new(Self {
            incarnation: binding_key.incarnation.clone(),
            binding_key,
            claims: std::sync::OnceLock::new(),
            module_id: module.into(),
            authority: crate::spawn_authority::SpawnAuthorityRecord::new(
                incarnation,
                generation,
                module,
            ),
            role,
            hello_seen: AtomicBool::new(false),
            registered: AtomicBool::new(false),
            nonce_registered: AtomicBool::new(false),
            deadline: std::sync::OnceLock::new(),
            launch,
            transition: Mutex::new(()),
            primary_sink: Mutex::new(None),
            #[cfg(test)]
            retirement_pause: Mutex::new(None),
            #[cfg(any(unix, windows))]
            endpoint: Mutex::new(None),
            binding,
            requires_binding,
            identity: std::sync::OnceLock::new(),
            connections: Arc::downgrade(&handle.registration_connections),
        });
        if let Some(binding) = &spawn.binding {
            binding.attach_registration(&spawn);
        }
        let mut launches = handle
            .registration_launches
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        launches.retain(|launch| launch.strong_count() != 0);
        launches.push(Arc::downgrade(&spawn));
        spawn
    }

    pub(crate) fn bind(self: &Arc<Self>, connection: ConnectionId) {
        if let Some(connections) = self.connections.upgrade() {
            connections
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(connection, self.clone());
        }
    }

    pub(super) fn ready(&self) {
        self.launch.send_if_modified(|state| {
            if *state == LaunchState::Pending {
                *state = LaunchState::Ready;
                true
            } else {
                false
            }
        });
    }
    pub(super) fn refuse(&self) {
        self.retire();
    }
    /// The one retirement step, run at the first exit observation (and when
    /// the launch is refused or its primary registration ends): the authority
    /// record dies, the claims are voided, the channel is shut and the epoch
    /// is bumped, synchronously and before the caller's next await.
    pub(crate) fn retire(&self) {
        self.retire_transition(
            self.transition.lock().unwrap_or_else(|p| p.into_inner()),
            true,
        );
    }

    // HELLO publication holds the transition guard, so retirement cannot miss
    // the primary sink or race a replacement into this slot after Dead.
    pub(crate) fn bind_primary_sink(&self, sink: crate::FrameSink) {
        *self.primary_sink.lock().unwrap_or_else(|p| p.into_inner()) = Some(sink);
    }

    pub(crate) fn end_primary_registration(&self) {
        let _transition = self.transition.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(claims) = self.claims.get() {
            claims.end_primary_registration();
        }
    }

    fn retire_unused(&self) {
        let transition = self.transition.lock().unwrap_or_else(|p| p.into_inner());
        if !self.registered() {
            self.nonce_registered.store(true, Ordering::Release);
            // The spawn registered by nonce instead: only its unused channel
            // ends. Its authority record stays live for its nonce consumers.
            self.retire_transition(transition, false);
        }
    }

    fn retire_transition(&self, transition: std::sync::MutexGuard<'_, ()>, ends_authority: bool) {
        let revoked;
        // Marking the record dead comes first; dropping the fence at the end
        // of this block bumps its epoch after the claims and channel ended.
        let authority_fence = ends_authority
            .then(|| self.authority.begin_retirement())
            .flatten();
        {
            if *self.launch.borrow() == LaunchState::Dead {
                return;
            }
            self.launch.send_replace(LaunchState::Dead);
            if let Some(primary) = self
                .primary_sink
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
            {
                primary.terminate_ingress();
                primary.terminate_egress();
            }
            revoked = self.claims.get().map(|claims| claims.revoke());
            #[cfg(any(unix, windows))]
            if let Some(endpoint) = self
                .endpoint
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
            {
                #[cfg(unix)]
                let _ = endpoint.shutdown(std::net::Shutdown::Both);
                #[cfg(windows)]
                endpoint.shutdown();
            }
        }
        drop(authority_fence);
        drop(transition);
        #[cfg(test)]
        if let Some(pause) = self.retirement_pause.lock().unwrap().take() {
            let _ = pause.reached.send(());
            let _ = pause.release.recv_timeout(Duration::from_secs(5));
        }
        if let (Some(claims), Some(revoked)) = (self.claims.get(), revoked) {
            claims.close_revoked(revoked);
        }
        if let Some(binding) = &self.binding {
            binding.retire();
        }
    }

    pub(crate) async fn wait_launch(&self) {
        let mut state = self.launch.subscribe();
        while *state.borrow_and_update() == LaunchState::Pending {
            if state.changed().await.is_err() {
                break;
            }
        }
    }

    pub(crate) fn launch_ready(&self) -> bool {
        *self.launch.borrow() == LaunchState::Ready
    }

    pub(crate) fn deadline(&self) -> Instant {
        *self
            .deadline
            .get()
            .expect("served registration has a spawn deadline")
    }

    pub(crate) fn registered(&self) -> bool {
        self.registered.load(Ordering::Acquire)
    }

    /// Serialize the registry/forwarding publication with synchronous retirement.
    /// Image verification happens before this lock; only snapshot writes follow it.
    pub(crate) fn begin_publication(&self) -> Result<std::sync::MutexGuard<'_, ()>, &'static str> {
        let guard = self.transition.lock().unwrap_or_else(|p| p.into_inner());
        if !self.launch_ready() {
            return Err(REGISTRATION_CHANNEL_CLOSED);
        }
        Ok(guard)
    }

    pub(crate) async fn registration_expired(&self) {
        tokio::time::sleep_until(self.deadline()).await;
        if self.registered.load(Ordering::Acquire) {
            std::future::pending::<()>().await;
        }
    }

    pub(crate) async fn wait_closed(&self) {
        let mut state = self.launch.subscribe();
        loop {
            if matches!(*state.borrow_and_update(), LaunchState::Dead) {
                return;
            }
            if state.changed().await.is_err() {
                return;
            }
        }
    }

    pub(crate) async fn wait_registration_failure(&self) {
        // A launch without a required program binding can still register by
        // nonce through an older SDK. Closing its unused channel is not a
        // registration failure.
        if !self.requires_binding {
            std::future::pending::<()>().await;
        }
        self.wait_closed().await;
        if self.registered() || self.nonce_registered.load(Ordering::Acquire) {
            std::future::pending::<()>().await;
        }
    }

    pub(crate) fn admit_hello(&self, module: Option<&str>) -> Result<(), &'static str> {
        if self.hello_seen.swap(true, Ordering::AcqRel) {
            return Err("registration_channel_hello_repeated");
        }
        if module != Some(self.module_id.as_str()) {
            return Err("registration_channel_module_mismatch");
        }
        self.check()
    }

    pub(crate) fn check(&self) -> Result<(), &'static str> {
        self.check_state()?;
        if self
            .binding
            .as_ref()
            .is_some_and(|binding| binding.check().is_err())
        {
            return Err("program_refused");
        }
        Ok(())
    }

    fn verdict(&self) -> Result<(), &'static str> {
        self.check_state()?;
        if self
            .binding
            .as_ref()
            .is_some_and(|binding| binding.verdict().is_err())
        {
            return Err("program_refused");
        }
        self.check_state()
    }

    fn check_state(&self) -> Result<(), &'static str> {
        match *self.launch.borrow() {
            LaunchState::Dead => return Err(REGISTRATION_CHANNEL_CLOSED),
            LaunchState::Pending => return Err("config_delivery_deferred"),
            LaunchState::Ready => {}
        }
        if self.requires_binding && self.binding.is_none() {
            return Err("program_refused");
        }
        Ok(())
    }

    pub(crate) fn has_program_binding(&self) -> bool {
        self.binding.is_some()
    }

    pub(crate) fn is_candidate(&self) -> bool {
        self.role == SpawnRole::SwapCandidate
    }

    pub(crate) fn hello_accepted(&self) {
        self.registered.store(true, Ordering::Release);
        #[cfg(windows)]
        if let Some(binding) = &self.binding {
            binding.hello_accepted();
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RegistrationService {
    router: Weak<crate::Router>,
    auth: crate::server::ServerAuth,
}

impl SupervisorHandle {
    /// The daemon incarnation spawn generations and authority records belong to.
    pub(crate) fn daemon_incarnation(&self) -> String {
        self.spawn_events
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .daemon_incarnation
            .clone()
    }

    /// Remember which launch the nonce just minted for `spawn` belongs to. A
    /// launch that received no nonce (a gated, channel-only launch) records
    /// nothing.
    pub(super) fn record_nonce_launch(&self, spawn: &Arc<RegistrationSpawn>) {
        let nonce = match spawn.role {
            SpawnRole::Plain => self
                .spawn_nonces
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&spawn.module_id)
                .cloned(),
            SpawnRole::SwapCandidate => self
                .swaps
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&spawn.module_id)
                .map(|swap| swap.candidate_nonce.clone()),
        };
        let Some(nonce) = nonce.filter(|nonce| !nonce.is_empty()) else {
            return;
        };
        let mut launches = self
            .nonce_launches
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        launches.retain(|_, launch| launch.strong_count() != 0);
        launches.insert(nonce, Arc::downgrade(spawn));
    }

    /// The authority record of the launch whose nonce `presented` is, when it
    /// still attests `module_id` exactly as `spawned_consumer_authorized`
    /// decides. `None` for an unknown, superseded or gated nonce.
    pub(crate) fn spawned_consumer_record(
        &self,
        module_id: &str,
        presented: &str,
    ) -> Option<Arc<crate::spawn_authority::SpawnAuthorityRecord>> {
        if !self.spawned_consumer_authorized(module_id, presented) {
            return None;
        }
        self.nonce_launches
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(presented)
            .and_then(Weak::upgrade)
            .filter(|spawn| spawn.module_id == module_id)
            .map(|spawn| spawn.authority.clone())
    }

    pub(crate) fn retire_unused_registration_channel(&self, module: &str, candidate: bool) {
        // Successful registration claims the id even when legacy policy did
        // not require a nonce. Resolve its unused endpoint from our launch
        // history, not a credential supplied by that registration.
        let launch = self
            .registration_launches
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .rev()
            .filter_map(Weak::upgrade)
            .find(|spawn| spawn.module_id == module && spawn.is_candidate() == candidate);
        if let Some(launch) = launch {
            launch.retire_unused();
        }
    }
    #[cfg(test)]
    pub(crate) fn capture_registration_channels_for_test(&self) {
        *self.registration_test_channels.lock().unwrap() = Some(Vec::new());
    }

    #[cfg(test)]
    pub(crate) fn registration_for_test(&self, module: &str) -> Option<Arc<RegistrationSpawn>> {
        self.registration_test_channels
            .lock()
            .unwrap()
            .as_ref()?
            .iter()
            .rev()
            .find(|(spawn, _)| spawn.module_id == module)
            .map(|(spawn, _)| spawn.clone())
    }

    #[cfg(test)]
    pub(crate) fn registration_proof_for_test(&self, module: &str) -> Option<RegistrationProof> {
        let Some(channel) = self.registration_for_test(module) else {
            return self.spawn_nonce(module).map(Into::into);
        };
        Some(RegistrationProof {
            key: channel
                .binding
                .as_ref()
                .map(|binding| binding.key_for_test())
                .unwrap_or_else(|| format!("channel-fixture:{:p}", Arc::as_ptr(&channel))),
            channel: Some(channel),
        })
    }

    #[cfg(test)]
    pub(crate) fn candidate_registration_proof_for_test(
        &self,
        module: &str,
    ) -> Option<RegistrationProof> {
        let channel = self.registration_for_test(module)?;
        if !channel.is_candidate() {
            return None;
        }
        Some(RegistrationProof {
            key: channel.binding.as_ref()?.key_for_test(),
            channel: Some(channel),
        })
    }
    /// Both proofs name the registration connection, not merely its module ID.
    /// A process birth identity ties a channel to the current launch snapshot,
    /// excluding candidates before promotion and incumbents after promotion.
    pub(crate) fn registration_is_current_live(
        &self,
        registry: &Registry,
        module: &str,
        connection: ConnectionId,
        nonce: Option<&str>,
    ) -> bool {
        if !self.registration_snapshot_is_current_live(registry, module, connection, nonce) {
            return false;
        }
        if self
            .registration_spawn(connection)
            .is_some_and(|spawn| spawn.verdict().is_err())
        {
            return false;
        }
        self.registration_snapshot_is_current_live(registry, module, connection, nonce)
    }

    pub(crate) fn registration_snapshot_is_current_live(
        &self,
        registry: &Registry,
        module: &str,
        connection: ConnectionId,
        nonce: Option<&str>,
    ) -> bool {
        if !registry
            .get_module(module)
            .ok()
            .flatten()
            .is_some_and(|registration| registration.connection_id == connection)
        {
            return false;
        }
        let Some(child) = self.get(module) else {
            return false;
        };
        let Some(launch) = child.resource_launch() else {
            return false;
        };
        if !launch.enabled || !launch.alive {
            return false;
        }
        if let Some(spawn) = self.registration_spawn(connection) {
            return spawn.module_id == module
                && spawn.check_state().is_ok()
                && (!self.module_is_gated(module) || spawn.has_program_binding())
                && spawn.identity.get().is_some_and(|identity| {
                    launch.pid == Some(identity.pid) && launch.start == Some(identity.start_time)
                });
        }
        if self.module_is_gated(module) {
            return false;
        }
        nonce.is_some_and(|presented| {
            self.spawn_nonce(module).is_some_and(|current| {
                !presented.is_empty() && constant_time_eq(current.as_bytes(), presented.as_bytes())
            })
        })
    }

    pub(crate) fn install_registration_service(
        &self,
        router: Weak<crate::Router>,
        auth: crate::server::ServerAuth,
    ) {
        let mut service = self
            .registration_service
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        debug_assert!(service.is_none(), "registration service installed twice");
        if service.is_none() {
            *service = Some(RegistrationService { router, auth });
        }
    }

    pub(crate) fn registration_spawn(
        &self,
        connection: ConnectionId,
    ) -> Option<Arc<RegistrationSpawn>> {
        self.registration_connections
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&connection)
            .cloned()
    }

    pub(crate) fn forget_registration_connection(&self, connection: ConnectionId) {
        self.registration_connections
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&connection);
    }

    #[cfg(any(unix, windows))]
    pub(super) fn serve_registration(
        &self,
        channel: SpawnRegistrationChannel,
        spawn: Arc<RegistrationSpawn>,
        pid: u32,
        deadline: Instant,
    ) {
        let _ = spawn.deadline.set(deadline);
        #[cfg(unix)]
        let endpoint = channel.try_clone();
        #[cfg(windows)]
        let endpoint = channel.shutdown_handle();
        match endpoint {
            Ok(endpoint) => {
                let _transition = spawn.transition.lock().unwrap_or_else(|p| p.into_inner());
                if *spawn.launch.borrow() == LaunchState::Dead {
                    #[cfg(unix)]
                    let _ = endpoint.shutdown(std::net::Shutdown::Both);
                    #[cfg(windows)]
                    endpoint.shutdown();
                    return;
                }
                *spawn.endpoint.lock().unwrap_or_else(|p| p.into_inner()) = Some(endpoint);
            }
            Err(_) => {
                spawn.refuse();
                return;
            }
        }
        #[cfg(test)]
        {
            let mut captured = self.registration_test_channels.lock().unwrap();
            if let Some(channels) = &mut *captured {
                channels.push((
                    spawn,
                    super::CapturedRegistrationEndpoint { _channel: channel },
                ));
                return;
            }
        }
        let service = self
            .registration_service
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let Some((router, auth)) =
            service.and_then(|service| Some((service.router.upgrade()?, service.auth)))
        else {
            // No router is available to authorize registration, so the spawn
            // is refused and the channel is closed without trying TCP or a nonce.
            spawn.refuse();
            #[cfg(unix)]
            let _ = channel.shutdown(std::net::Shutdown::Both);
            return;
        };
        tokio::spawn(async move {
            let serve = async {
                #[cfg(unix)]
                let stream = {
                    let _ = pid;
                    channel.set_nonblocking(true)?;
                    tokio::net::UnixStream::from_std(channel)?
                };
                #[cfg(windows)]
                let stream = {
                    tokio::task::spawn_blocking(move || {
                        channel.accept_tokio_pipe_server(pid, deadline.into_std())
                    })
                    .await
                    .map_err(io::Error::other)??
                };
                let _ = crate::server::handle_connection_with_origin(
                    RegistrationStream {
                        stream,
                        spawn: spawn.clone(),
                    },
                    router,
                    auth,
                    crate::server::LocalOrigin::RegistrationChannel(spawn.clone()),
                )
                .await;
                Ok::<(), io::Error>(())
            };
            let _ = serve.await;
            spawn.retire();
        });
    }
}

// Retire at the transport's first EOF, including during authentication or frame
// decoding, rather than waiting for the connection loop to finish unwinding.
// Interrupted skips only this transport-level revoke: frame and authentication
// readers propagate it, so their connection cleanup still retires the launch.
struct RegistrationStream<S> {
    stream: S,
    spawn: Arc<RegistrationSpawn>,
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for RegistrationStream<S> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let can_read = buf.remaining() != 0;
        let result = std::pin::Pin::new(&mut this.stream).poll_read(cx, buf);
        let ended = match &result {
            std::task::Poll::Ready(Ok(())) => can_read && buf.filled().len() == before,
            std::task::Poll::Ready(Err(error)) => error.kind() != io::ErrorKind::Interrupted,
            std::task::Poll::Pending => false,
        };
        if ended {
            this.spawn.retire();
        }
        result
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for RegistrationStream<S> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().stream).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}

/// A unit fixture owns the daemon endpoint while it dispatches HELLO directly,
/// without a socket reader. Its table key locates a channel; it is not sent as a
/// launch nonce. Full transport tests use the authenticated service instead.
#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct RegistrationProof {
    key: String,
    channel: Option<Arc<RegistrationSpawn>>,
}
#[cfg(test)]
impl std::ops::Deref for RegistrationProof {
    type Target = str;
    fn deref(&self) -> &str {
        &self.key
    }
}
#[cfg(test)]
impl PartialEq for RegistrationProof {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
#[cfg(test)]
impl From<String> for RegistrationProof {
    fn from(key: String) -> Self {
        Self { key, channel: None }
    }
}
#[cfg(test)]
impl From<&str> for RegistrationProof {
    fn from(key: &str) -> Self {
        key.to_owned().into()
    }
}
#[cfg(test)]
impl RegistrationProof {
    pub(crate) fn bind(&self, connection: ConnectionId) {
        if let Some(channel) = &self.channel {
            channel.bind(connection);
        }
    }
    pub(crate) fn nonce(&self) -> Option<String> {
        self.channel.is_none().then(|| self.key.clone())
    }
    #[cfg(target_os = "linux")]
    pub(crate) fn as_str(&self) -> &str {
        &self.key
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use subc_protocol::{Frame, FrameType};

    fn hello(module: &str) -> Frame {
        let manifest = subc_protocol::manifest::ModuleManifest::builder(module, "0.1.0")
            .protocol_ver(subc_protocol::PROTOCOL_VERSION)
            .build();
        Frame::build(
            FrameType::Hello,
            subc_protocol::Flags::new(false, subc_protocol::Priority::Interactive, false),
            0,
            0,
            10,
            serde_json::to_vec(
                &json!({"protocol_ver": subc_protocol::PROTOCOL_VERSION, "manifest": manifest}),
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn code(frames: &[Frame]) -> String {
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].header.ty, FrameType::Error);
        serde_json::from_slice::<serde_json::Value>(&frames[0].body).unwrap()["code"]
            .as_str()
            .unwrap()
            .into()
    }

    fn setup() -> (SupervisorHandle, Arc<Registry>, crate::ControlHandler) {
        let handle = SupervisorHandle::new();
        let registry = Arc::new(Registry::default());
        let control = crate::ControlHandler::new(registry.clone()).with_supervisor(handle.clone());
        (handle, registry, control)
    }

    fn bound(handle: &SupervisorHandle, id: u64, role: SpawnRole) -> Arc<RegistrationSpawn> {
        let spawn = RegistrationSpawn::new(handle, "channel-module", role, None);
        spawn.ready();
        spawn.bind(ConnectionId::new(id));
        spawn
    }

    #[cfg(unix)]
    async fn hello_rollback_case(candidate: bool) {
        let worker = if candidate {
            "supervise::registration::tests::candidate_hello_forwarding_rollback_worker"
        } else {
            "supervise::registration::tests::ordinary_hello_forwarding_rollback_worker"
        };
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", worker, "--ignored", "--nocapture"])
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        if result.is_err() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        assert!(matches!(result, Ok(Ok(status)) if status.success()),
            "channel HELLO rollback must finish and allow later retirement within the subprocess deadline");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ordinary_channel_hello_rollback_does_not_reenter_transition() {
        hello_rollback_case(false).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn candidate_channel_hello_rollback_does_not_reenter_transition() {
        hello_rollback_case(true).await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "subprocess fixture; invoked only by the bounded rollback tests"]
    async fn ordinary_hello_forwarding_rollback_worker() {
        hello_forwarding_rollback_worker(false).await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "subprocess fixture; invoked only by the bounded rollback tests"]
    async fn candidate_hello_forwarding_rollback_worker() {
        hello_forwarding_rollback_worker(true).await;
    }

    #[cfg(unix)]
    async fn hello_forwarding_rollback_worker(candidate: bool) {
        let (handle, registry, control) = setup();
        if candidate {
            assert_eq!(
                control
                    .handle_control(ConnectionId::new(90), hello("channel-module"))
                    .unwrap()[0]
                    .header
                    .ty,
                FrameType::HelloAck
            );
            handle.open_swap("channel-module", "candidate".into());
        }
        let forwarding = control.forwarding();
        forwarding.begin_daemon_drain().unwrap();
        let role = if candidate {
            SpawnRole::SwapCandidate
        } else {
            SpawnRole::Plain
        };
        let spawn = RegistrationSpawn::new(&handle, "channel-module", role, None);
        spawn.ready();
        let router = Arc::new(crate::Router::with_control_handler(Arc::new(control)));
        let info = subc_transport::ConnectionInfo {
            schema: subc_transport::SCHEMA_VERSION,
            wire_version: None,
            endpoints: Vec::new(),
            key: vec![0x42; 32],
            daemon_id: [0x24; 16],
            pid: std::process::id(),
            daemon_ver: "test".into(),
        };
        router.install_registration_service(crate::server::ServerAuth::new(
            info.key.clone(),
            info.daemon_id,
            "test",
        ));
        let (parent, child) = std::os::unix::net::UnixStream::pair().unwrap();
        child.set_nonblocking(true).unwrap();
        let mut child = tokio::net::UnixStream::from_std(child).unwrap();
        handle.serve_registration(
            parent,
            spawn.clone(),
            std::process::id(),
            Instant::now() + Duration::from_secs(5),
        );
        subc_transport::authenticate_client(&mut child, &info, Duration::from_secs(1))
            .await
            .unwrap();
        subc_transport::write_frame(&mut child, &hello("channel-module"))
            .await
            .unwrap();
        let reply = tokio::time::timeout(
            Duration::from_secs(1),
            subc_transport::read_frame(&mut child),
        )
        .await
        .expect("forwarding refusal must settle the channel HELLO")
        .unwrap();
        if let Some(reply) = reply {
            assert_eq!(reply.header.ty, FrameType::Error);
        }
        assert!(!spawn.registered());
        if candidate {
            assert!(registry.get_candidate("channel-module").unwrap().is_none());
        } else {
            assert!(registry.get_module("channel-module").unwrap().is_none());
        }
        tokio::time::timeout(
            Duration::from_secs(1),
            tokio::task::spawn_blocking(move || spawn.retire()),
        )
        .await
        .expect("retirement after HELLO rollback must return")
        .unwrap();
    }

    #[test]
    fn registration_channel_wrong_id_consumes_hello() {
        let (handle, registry, control) = setup();
        let _spawn = bound(&handle, 1, SpawnRole::Plain);
        assert_eq!(
            code(
                &control
                    .handle_control(ConnectionId::new(1), hello("another-module"))
                    .unwrap()
            ),
            "registration_channel_module_mismatch"
        );
        assert_eq!(
            code(
                &control
                    .handle_control(ConnectionId::new(1), hello("channel-module"))
                    .unwrap()
            ),
            "registration_channel_hello_repeated"
        );
        assert_eq!(registry.active_registration_count().unwrap(), 0);
    }

    #[test]
    fn registration_channel_second_hello_is_refused() {
        let (handle, registry, control) = setup();
        let _spawn = bound(&handle, 1, SpawnRole::Plain);
        let replies = control
            .handle_control(ConnectionId::new(1), hello("channel-module"))
            .unwrap();
        assert_eq!(
            replies[0].header.ty,
            FrameType::HelloAck,
            "{}",
            String::from_utf8_lossy(&replies[0].body)
        );
        assert_eq!(
            code(
                &control
                    .handle_control(ConnectionId::new(1), hello("channel-module"))
                    .unwrap()
            ),
            "registration_channel_hello_repeated"
        );
        assert_eq!(registry.active_registration_count().unwrap(), 1);
    }

    #[test]
    fn registration_channel_hello_after_exit_is_refused() {
        let (handle, registry, control) = setup();
        let spawn = bound(&handle, 1, SpawnRole::Plain);
        spawn.retire();
        assert_eq!(
            code(
                &control
                    .handle_control(ConnectionId::new(1), hello("channel-module"))
                    .unwrap()
            ),
            "registration_channel_closed"
        );
        assert_eq!(registry.active_registration_count().unwrap(), 0);
    }

    #[test]
    fn registration_channel_plain_respawn_during_swap_preserves_draining_slot() {
        let (handle, registry, control) = setup();
        let _incumbent = bound(&handle, 1, SpawnRole::Plain);
        assert_eq!(
            control
                .handle_control(ConnectionId::new(1), hello("channel-module"))
                .unwrap()[0]
                .header
                .ty,
            FrameType::HelloAck
        );
        handle.open_swap("channel-module", "legacy-swap-token".into());
        let _candidate = bound(&handle, 2, SpawnRole::SwapCandidate);
        assert_eq!(
            control
                .handle_control(ConnectionId::new(2), hello("channel-module"))
                .unwrap()[0]
                .header
                .ty,
            FrameType::HelloAck
        );
        registry
            .promote_candidate("channel-module")
            .unwrap()
            .unwrap();
        control.cleanup_connection(ConnectionId::new(2)).unwrap();
        assert_eq!(
            handle.swap_hello_admission("channel-module", None),
            SwapHelloAdmission::Refused
        );
        let _respawn = bound(&handle, 3, SpawnRole::Plain);
        assert_eq!(
            control
                .handle_control(ConnectionId::new(3), hello("channel-module"))
                .unwrap()[0]
                .header
                .ty,
            FrameType::HelloAck
        );
        assert_eq!(
            registry
                .get_module("channel-module")
                .unwrap()
                .unwrap()
                .connection_id,
            ConnectionId::new(3)
        );
        assert!(registry
            .get_module_by_connection(ConnectionId::new(1))
            .unwrap()
            .is_some());
        assert!(registry.list_modules_with_nonce_verification().unwrap().1[0].1);
        control.cleanup_connection(ConnectionId::new(1)).unwrap();
        assert_eq!(
            registry
                .get_module("channel-module")
                .unwrap()
                .unwrap()
                .connection_id,
            ConnectionId::new(3)
        );
    }

    fn auth() -> (crate::server::ServerAuth, subc_transport::ConnectionInfo) {
        let key = vec![0x42; 32];
        let daemon_id = [0x24; 16];
        let info = subc_transport::ConnectionInfo {
            schema: subc_transport::SCHEMA_VERSION,
            wire_version: None,
            endpoints: Vec::new(),
            key: key.clone(),
            daemon_id,
            pid: std::process::id(),
            daemon_ver: "test".into(),
        };
        (crate::server::ServerAuth::new(key, daemon_id, "test"), info)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn registration_channel_service_reuses_authenticated_frames_and_never_reconnects() {
        let (handle, registry, _) = setup();
        let forwarding = Arc::new(crate::ForwardingTable::default());
        let control = crate::ControlHandler::with_forwarding(registry.clone(), forwarding.clone())
            .with_supervisor(handle.clone());
        let router = Arc::new(crate::Router::with_control_handler(Arc::new(control)));
        let (auth, info) = auth();
        router.install_registration_service(auth);
        let spawn = RegistrationSpawn::new(&handle, "channel-module", SpawnRole::Plain, None);
        spawn.ready();
        let (parent, child) = std::os::unix::net::UnixStream::pair().unwrap();
        child.set_nonblocking(true).unwrap();
        let mut child = tokio::net::UnixStream::from_std(child).unwrap();
        handle.serve_registration(
            parent,
            spawn.clone(),
            std::process::id(),
            Instant::now() + Duration::from_secs(2),
        );
        subc_transport::authenticate_client(&mut child, &info, Duration::from_secs(2))
            .await
            .unwrap();
        let mut body: subc_protocol::ModuleHelloBody =
            serde_json::from_slice(&hello("channel-module").body).unwrap();
        body.control_ops = Some(vec!["health.check".into()]);
        let registration = Frame::build(
            FrameType::Hello,
            super::super::control_flags(),
            0,
            0,
            1,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
        subc_transport::write_frame(&mut child, &registration)
            .await
            .unwrap();
        assert_eq!(
            subc_transport::read_frame(&mut child)
                .await
                .unwrap()
                .unwrap()
                .header
                .ty,
            FrameType::HelloAck
        );
        assert!(
            registry.list_modules_with_nonce_verification().unwrap().1[0].1,
            "the channel itself proves the spawn identity without a body nonce"
        );
        let ping = Frame::build(
            FrameType::Ping,
            subc_protocol::Flags::new(false, subc_protocol::Priority::Interactive, false),
            0,
            0,
            11,
            Vec::new(),
        )
        .unwrap();
        subc_transport::write_frame(&mut child, &ping)
            .await
            .unwrap();
        assert_eq!(
            subc_transport::read_frame(&mut child)
                .await
                .unwrap()
                .unwrap()
                .header
                .ty,
            FrameType::Pong
        );
        let request = Frame::build(
            FrameType::Request,
            subc_protocol::Flags::new(false, subc_protocol::Priority::Interactive, false),
            0,
            0,
            12,
            serde_json::to_vec(
                &subc_protocol::session::ModuleControlRequestFromModule::CatalogUpdate {
                    provides: Vec::new(),
                    capabilities: None,
                    ready: Some(true),
                },
            )
            .unwrap(),
        )
        .unwrap();
        subc_transport::write_frame(&mut child, &request)
            .await
            .unwrap();
        assert_eq!(
            subc_transport::read_frame(&mut child)
                .await
                .unwrap()
                .unwrap()
                .header
                .ty,
            FrameType::Response
        );
        let started = tokio::time::Instant::now();
        let pending = forwarding
            .begin_health_probe_rpc_for(
                "channel-module",
                "health.check",
                started,
                started + Duration::from_secs(2),
            )
            .unwrap();
        let health = Frame::build(
            FrameType::Request,
            super::super::control_flags(),
            0,
            0,
            pending.corr,
            serde_json::to_vec(&subc_protocol::session::ModuleControlRequest::HealthCheck {})
                .unwrap(),
        )
        .unwrap();
        pending.module_sink.send(health).await.unwrap();
        let health = tokio::time::timeout(
            Duration::from_secs(2),
            subc_transport::read_frame(&mut child),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&health.body).unwrap(),
            json!({"op":"health.check"})
        );
        let reply =
            json!({"op":"health.check", "status":"degraded", "detail":"channel health reply"});
        let response = Frame::build(
            FrameType::Response,
            super::super::control_flags(),
            0,
            0,
            health.header.corr,
            serde_json::to_vec(&reply).unwrap(),
        )
        .unwrap();
        subc_transport::write_frame(&mut child, &response)
            .await
            .unwrap();
        match tokio::time::timeout(Duration::from_secs(2), pending.receiver)
            .await
            .unwrap()
            .unwrap()
        {
            crate::forwarding::ModuleControlRpcOutcome::Response(response) => {
                assert_eq!(serde_json::to_value(response).unwrap(), reply)
            }
            other => panic!("health reply did not traverse the channel: {other:?}"),
        }
        let push_body =
            serde_json::to_vec(&subc_protocol::config::ConfigChanged::new(7, Vec::new())).unwrap();
        let push = Frame::build(
            FrameType::Push,
            super::super::control_flags(),
            0,
            0,
            0,
            push_body.clone(),
        )
        .unwrap();
        pending.module_sink.send(push).await.unwrap();
        let pushed = tokio::time::timeout(
            Duration::from_secs(2),
            subc_transport::read_frame(&mut child),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(pushed.header.ty, FrameType::Push);
        assert_eq!(pushed.body, push_body);
        drop(pending.module_sink);
        spawn.retire();
        assert!(tokio::time::timeout(
            Duration::from_secs(2),
            subc_transport::read_frame(&mut child)
        )
        .await
        .unwrap()
        .unwrap()
        .is_none());
        tokio::time::timeout(Duration::from_secs(2), async {
            while registry.get_module("channel-module").unwrap().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn registration_channel_missing_or_gone_service_closes_without_nonce_fallback() {
        let (handle, _, control) = setup();
        for installed in [false, true] {
            if installed {
                let router = Arc::new(crate::Router::with_control_handler(Arc::new(
                    control.clone(),
                )));
                router.install_registration_service(auth().0);
                drop(router);
            }
            let spawn = RegistrationSpawn::new(&handle, "channel-module", SpawnRole::Plain, None);
            let (parent, child) = std::os::unix::net::UnixStream::pair().unwrap();
            child.set_nonblocking(true).unwrap();
            let mut child = tokio::net::UnixStream::from_std(child).unwrap();
            handle.serve_registration(
                parent,
                spawn.clone(),
                1,
                Instant::now() + Duration::from_secs(1),
            );
            use tokio::io::AsyncReadExt;
            assert_eq!(child.read(&mut [0]).await.unwrap(), 0);
            assert_eq!(spawn.check(), Err("registration_channel_closed"));
            assert!(handle.spawn_nonce("channel-module").is_none());
        }
    }

    #[test]
    fn registration_channel_service_installs_only_once() {
        let (handle, _, control) = setup();
        let router = Arc::new(crate::Router::with_control_handler(Arc::new(control)));
        router.install_registration_service(auth().0);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            router.install_registration_service(auth().0)
        }));
        assert_eq!(result.is_err(), cfg!(debug_assertions));
        assert!(handle
            .registration_service
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn registration_channel_macos_hello_waits_for_verified_transition_and_refusal_closes() {
        for refused in [false, true] {
            let (handle, _, control) = setup();
            let router = Arc::new(crate::Router::with_control_handler(Arc::new(control)));
            let (auth, info) = auth();
            router.install_registration_service(auth);
            let roster = ChildRoster::default();
            roster.set_privacy_trampoline(Ok(test_privacy_trampoline()));
            let scratch = cortexkit_test_support::ScratchDir::new("channel-launch");
            let spec = ModuleSpec {
                module_id: "channel-module".into(),
                program: "/bin/sleep".into(),
                args: vec!["60".into()],
                env: [
                    "HOME",
                    "XDG_DATA_HOME",
                    "XDG_RUNTIME_DIR",
                    "XDG_CONFIG_HOME",
                ]
                .into_iter()
                .map(|key| (key.into(), scratch.display().to_string()))
                .collect(),
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::Subc,
                overlap: Default::default(),
                carry_containment: false,
            };
            let mut spec = spec;
            spec.protocol = ModuleProtocol::None;
            let owner = super::super::suspended_spawn_tests::gated_owner(&scratch, &spec, true);
            let mut runtime =
                Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
                    .with_config_runtime(owner)
                    .runtime_config();
            runtime.child_roster = roster;
            let mut child = spawn_child_in_slot_async(&spec, &runtime, SpawnRole::Plain, false)
                .await
                .unwrap();
            let spawn = RegistrationSpawn::new(&handle, "channel-module", SpawnRole::Plain, None);
            child.registration_spawn = Some(spawn.clone());
            let (parent, peer) = std::os::unix::net::UnixStream::pair().unwrap();
            peer.set_nonblocking(true).unwrap();
            let mut peer = tokio::net::UnixStream::from_std(peer).unwrap();
            handle.serve_registration(
                parent,
                spawn,
                child.pid,
                Instant::now() + Duration::from_secs(2),
            );
            subc_transport::authenticate_client(&mut peer, &info, Duration::from_secs(2))
                .await
                .unwrap();
            subc_transport::write_frame(&mut peer, &hello("channel-module"))
                .await
                .unwrap();
            let pid = child.pid;
            tokio::time::timeout(Duration::from_secs(2), async {
                while !subc_os::privacy_identity::SuspendedSignature::is_suspended(pid).unwrap() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            // The exec-acknowledgement reader returns EOF and the PID has not changed. Neither fact
            // authorizes HELLO before signature verification and resume.
            let reader = &child.privacy_exec.as_ref().unwrap().reader;
            let mut readiness = reader.readable().await.unwrap();
            assert_eq!(
                readiness
                    .try_io(|reader| {
                        use std::io::Read;
                        reader.get_ref().read(&mut [0])
                    })
                    .unwrap()
                    .unwrap(),
                0
            );
            assert_eq!(child.pid, pid);
            assert!(tokio::time::timeout(
                Duration::from_millis(50),
                subc_transport::read_frame(&mut peer)
            )
            .await
            .is_err());
            if refused {
                child.privacy_exec.as_mut().unwrap().expected = None;
            }
            child.confirm_privacy_exec().await;
            let reply = tokio::time::timeout(
                Duration::from_secs(2),
                subc_transport::read_frame(&mut peer),
            )
            .await
            .unwrap()
            .unwrap();
            if refused {
                assert!(reply.is_none());
            } else {
                assert_eq!(reply.unwrap().header.ty, FrameType::HelloAck);
            }
            let _ = child.start_admission_kill();
            child.wait().await.unwrap();
        }
    }
}
