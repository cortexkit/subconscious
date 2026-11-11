//! Per-launch registration, tracked separately from nonce checks keyed by module ID.
//! Locks only protect snapshots; neither service nor launch waits hold them.

#[cfg(all(test, unix))]
#[path = "registration_review_tests.rs"]
mod review_tests;

use super::*;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Weak,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchState {
    Pending,
    Ready,
    Dead,
}

#[derive(Debug)]
pub(crate) struct RegistrationSpawn {
    pub(crate) module_id: String,
    pub(super) role: SpawnRole,
    hello_seen: AtomicBool,
    registered: AtomicBool,
    deadline: std::sync::OnceLock<Instant>,
    launch: tokio::sync::watch::Sender<LaunchState>,
    transition: Mutex<()>,
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
        let spawn = Arc::new(Self {
            module_id: module.into(),
            role,
            hello_seen: AtomicBool::new(false),
            registered: AtomicBool::new(false),
            deadline: std::sync::OnceLock::new(),
            launch,
            transition: Mutex::new(()),
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
    pub(crate) fn retire(&self) {
        {
            let _transition = self.transition.lock().unwrap_or_else(|p| p.into_inner());
            if *self.launch.borrow() == LaunchState::Dead {
                return;
            }
            self.launch.send_replace(LaunchState::Dead);
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
        match *self.launch.borrow() {
            LaunchState::Dead => return Err("registration_channel_closed"),
            LaunchState::Pending => return Err("config_delivery_deferred"),
            LaunchState::Ready => {}
        }
        if self.requires_binding && self.binding.is_none() {
            return Err("program_refused");
        }
        if self
            .binding
            .as_ref()
            .is_some_and(|binding| binding.check().is_err())
        {
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
    #[cfg(all(test, unix))]
    pub(crate) fn capture_registration_channels_for_test(&self) {
        *self.registration_test_channels.lock().unwrap() = Some(Vec::new());
    }

    #[cfg(all(test, unix))]
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

    #[cfg(all(test, unix))]
    pub(crate) fn registration_proof_for_test(&self, module: &str) -> Option<RegistrationProof> {
        let Some(channel) = self.registration_for_test(module) else {
            return self.spawn_nonce(module).map(Into::into);
        };
        Some(RegistrationProof {
            key: channel
                .binding
                .as_ref()
                .map(|binding| binding.key_for_test().to_owned())
                .unwrap_or_else(|| format!("channel-fixture:{:p}", Arc::as_ptr(&channel))),
            channel: Some(channel),
        })
    }

    #[cfg(all(test, unix))]
    pub(crate) fn candidate_registration_proof_for_test(
        &self,
        module: &str,
    ) -> Option<RegistrationProof> {
        let channel = self.registration_for_test(module)?;
        if !channel.is_candidate() {
            return None;
        }
        Some(RegistrationProof {
            key: channel.binding.as_ref()?.key_for_test().into(),
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
                && spawn.check().is_ok()
                && spawn.identity.get().is_some_and(|identity| {
                    launch.pid == Some(identity.pid) && launch.start == Some(identity.start_time)
                });
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
        #[cfg(all(test, unix))]
        {
            let mut captured = self.registration_test_channels.lock().unwrap();
            if let Some(channels) = &mut *captured {
                channels.push((spawn, channel));
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
                    stream,
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

/// A unit fixture owns the daemon endpoint while it dispatches HELLO directly,
/// without a socket reader. Its table key locates a channel; it is not sent as a
/// launch nonce. Full transport tests use the authenticated service instead.
#[cfg(all(test, unix))]
#[derive(Clone, Debug)]
pub(crate) struct RegistrationProof {
    key: String,
    channel: Option<Arc<RegistrationSpawn>>,
}
#[cfg(all(test, unix))]
impl std::ops::Deref for RegistrationProof {
    type Target = str;
    fn deref(&self) -> &str {
        &self.key
    }
}
#[cfg(all(test, unix))]
impl PartialEq for RegistrationProof {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
#[cfg(all(test, unix))]
impl From<String> for RegistrationProof {
    fn from(key: String) -> Self {
        Self { key, channel: None }
    }
}
#[cfg(all(test, unix))]
impl From<&str> for RegistrationProof {
    fn from(key: &str) -> Self {
        key.to_owned().into()
    }
}
#[cfg(all(test, unix))]
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
