use super::*;

fn auth() -> (crate::server::ServerAuth, subc_transport::ConnectionInfo) {
    let info = subc_transport::ConnectionInfo {
        schema: subc_transport::SCHEMA_VERSION,
        wire_version: None,
        endpoints: Vec::new(),
        key: vec![0x42; 32],
        daemon_id: [0x24; 16],
        pid: std::process::id(),
        daemon_ver: "test".into(),
    };
    (
        crate::server::ServerAuth::new(info.key.clone(), info.daemon_id, "test"),
        info,
    )
}

fn serve(
    handle: &SupervisorHandle,
    spawn: Arc<RegistrationSpawn>,
    auth: crate::server::ServerAuth,
) -> (
    Arc<crate::Router>,
    tokio::net::UnixStream,
    std::os::unix::net::UnixStream,
) {
    let control =
        crate::ControlHandler::new(Arc::new(Registry::default())).with_supervisor(handle.clone());
    let router = Arc::new(crate::Router::with_control_handler(Arc::new(control)));
    router.install_registration_service(auth);
    let (parent, child) = std::os::unix::net::UnixStream::pair().unwrap();
    let retained = parent.try_clone().unwrap();
    child.set_nonblocking(true).unwrap();
    let child = tokio::net::UnixStream::from_std(child).unwrap();
    handle.serve_registration(parent, spawn, std::process::id(), Duration::from_secs(5));
    (router, child, retained)
}

#[tokio::test]
async fn registration_channel_binding_retirement_shuts_down_retained_endpoint() {
    let handle = SupervisorHandle::new();
    handle.set_spawn_nonce("channel-module", "binding-key".into());
    let publication = handle
        .begin_program_binding("channel-module", SpawnRole::Plain)
        .unwrap();
    let spawn = RegistrationSpawn::new(
        &handle,
        "channel-module",
        SpawnRole::Plain,
        Some(publication.clone()),
    );
    spawn.ready();
    let (auth, info) = auth();
    let (_router, mut child, _inherited_daemon_copy) = serve(&handle, spawn.clone(), auth);
    subc_transport::authenticate_client(&mut child, &info, Duration::from_secs(2))
        .await
        .unwrap();
    publication.retire();
    assert!(tokio::time::timeout(
        Duration::from_secs(1),
        subc_transport::read_frame(&mut child)
    )
    .await
    .expect("retirement must shut down even a retained daemon endpoint")
    .unwrap()
    .is_none());
    tokio::time::timeout(Duration::from_secs(1), spawn.wait_closed())
        .await
        .unwrap();
    assert!(!spawn.launch_ready());
}
