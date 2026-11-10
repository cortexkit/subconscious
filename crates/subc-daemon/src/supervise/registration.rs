//! Per-launch registration, tracked separately from nonce checks keyed by module ID.
//! Locks only protect snapshots; neither service nor launch waits hold them.

use super::*;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Weak,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchState {
    Pending,
    Ready,
    Refused,
    Dead,
}

#[derive(Debug)]
pub(crate) struct RegistrationSpawn {
    pub(crate) module_id: String,
    pub(super) role: SpawnRole,
    hello_seen: AtomicBool,
    launch: tokio::sync::watch::Sender<LaunchState>,
    binding: Option<ProgramBindingPublication>,
    connections: Weak<Mutex<HashMap<ConnectionId, Arc<RegistrationSpawn>>>>,
}

impl RegistrationSpawn {
    pub(super) fn new(
        handle: &SupervisorHandle,
        module: &str,
        role: SpawnRole,
        binding: Option<ProgramBindingPublication>,
    ) -> Arc<Self> {
        let (launch, _) = tokio::sync::watch::channel(LaunchState::Pending);
        Arc::new(Self {
            module_id: module.into(),
            role,
            hello_seen: AtomicBool::new(false),
            launch,
            binding,
            connections: Arc::downgrade(&handle.registration_connections),
        })
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
        self.launch.send_replace(LaunchState::Refused);
    }
    pub(crate) fn retire(&self) {
        self.launch.send_replace(LaunchState::Dead);
    }

    pub(crate) async fn wait_launch(&self) {
        let mut state = self.launch.subscribe();
        while *state.borrow_and_update() == LaunchState::Pending {
            if state.changed().await.is_err() {
                break;
            }
        }
    }

    pub(crate) async fn wait_closed(&self) {
        let mut state = self.launch.subscribe();
        loop {
            if matches!(
                *state.borrow_and_update(),
                LaunchState::Dead | LaunchState::Refused
            ) {
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
            LaunchState::Refused => return Err("program_refused"),
            LaunchState::Pending => return Err("config_delivery_deferred"),
            LaunchState::Ready => {}
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

    pub(crate) fn is_candidate(&self) -> bool {
        self.role == SpawnRole::SwapCandidate
    }

    #[cfg(windows)]
    pub(crate) fn hello_accepted(&self) {
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
        timeout: Duration,
    ) {
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
                    let _ = (pid, timeout);
                    channel.set_nonblocking(true)?;
                    tokio::net::UnixStream::from_std(channel)?
                };
                #[cfg(windows)]
                let stream = {
                    use std::os::windows::io::IntoRawHandle;
                    let file = tokio::task::spawn_blocking(move || {
                        channel.accept(pid, std::time::Instant::now() + timeout)
                    })
                    .await
                    .map_err(io::Error::other)??;
                    // SAFETY: accept returns the uniquely owned, overlapped
                    // server handle after verifying the child's process id.
                    #[allow(unsafe_code)]
                    unsafe {
                        tokio::net::windows::named_pipe::NamedPipeServer::from_raw_handle(
                            file.into_raw_handle(),
                        )?
                    }
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
        let (handle, registry, control) = setup();
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
            Duration::from_secs(2),
        );
        subc_transport::authenticate_client(&mut child, &info, Duration::from_secs(2))
            .await
            .unwrap();
        subc_transport::write_frame(&mut child, &hello("channel-module"))
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
            serde_json::to_vec(&json!({"op":"catalog.update", "ready":true})).unwrap(),
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
        spawn.retire();
        assert!(tokio::time::timeout(
            Duration::from_secs(2),
            subc_transport::read_frame(&mut child)
        )
        .await
        .unwrap()
        .unwrap()
        .is_none());
        assert!(registry.get_module("channel-module").unwrap().is_none());
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
            handle.serve_registration(parent, spawn.clone(), 1, Duration::from_secs(1));
            use tokio::io::AsyncReadExt;
            assert_eq!(child.read(&mut [0]).await.unwrap(), 0);
            assert_eq!(spawn.check(), Err("program_refused"));
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
            let spec = ModuleSpec {
                module_id: "channel-module".into(),
                program: "/bin/sleep".into(),
                args: vec!["60".into()],
                env: Vec::new(),
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::Subc,
                overlap: Default::default(),
                carry_containment: false,
            };
            let ring = Arc::new(Mutex::new(StderrRing::new(StderrTailConfig::default())));
            let mut child = spawn_child_in_slot(
                &spec,
                None,
                None,
                &ring,
                None,
                &roster,
                None,
                SpawnRole::Plain,
                false,
            )
            .unwrap();
            let spawn = RegistrationSpawn::new(&handle, "channel-module", SpawnRole::Plain, None);
            child.registration_spawn = Some(spawn.clone());
            let (parent, peer) = std::os::unix::net::UnixStream::pair().unwrap();
            peer.set_nonblocking(true).unwrap();
            let mut peer = tokio::net::UnixStream::from_std(peer).unwrap();
            handle.serve_registration(parent, spawn, child.pid, Duration::from_secs(2));
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
            // The ack is empty EOF and the PID has not changed. Neither fact
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
