use super::*;
use cortexkit_test_support::ScratchDir;

fn config(root: &Path, runtime: Option<OsString>) -> BootstrapConfig {
    let source = connection_file_path_with_source(runtime).1;
    let mut config =
        BootstrapConfig::new(root.join("primary.json"), 0).with_connection_file_source(source);
    config.local_app_data_override = Some(root.join("local-app-data"));
    config
}

fn fixed(root: &Path) -> PathBuf {
    root.join("local-app-data/cortexkit/run/subc-connection.json")
}

fn bound(outcome: Outcome) -> BoundDaemon {
    match outcome {
        Outcome::Bound(bound) => bound,
        Outcome::AlreadyRunning => panic!("fixture unexpectedly found a running daemon"),
    }
}

fn serve(bound: BoundDaemon) -> JoinHandle<Result<(), ServerError>> {
    let info = &bound.connection_info;
    let auth = ServerAuth::new(info.key.clone(), info.daemon_id, info.daemon_ver.clone());
    tokio::spawn(serve_listeners(
        bound.listeners,
        Arc::new(Router::with_default_self_handler()),
        auth,
    ))
}

async fn publish(bound: &BoundDaemon) {
    let (result, _) = publish_connection_file(
        &bound.connection_file_path,
        bound.connection_file_copy.as_deref(),
        &bound.connection_info,
        bound.local_endpoint.as_ref(),
        (),
    )
    .await;
    result.unwrap();
}

#[tokio::test]
async fn windows_copy_matches_primary_when_runtime_is_unset() {
    let root = ScratchDir::new("windows-copy-publish");
    let config = config(&root, None).with_local_listener_socket_dir(root.join("local-listener"));
    let primary = config.connection_file_path.clone();
    let bound = bound(ensure_singleton_with_config(config).await.unwrap());
    assert!(bound.local_endpoint.is_some());
    assert_eq!(
        bound.connection_file_copy.as_deref(),
        Some(fixed(&root).as_path())
    );
    assert_eq!(fs::read(primary).unwrap(), fs::read(fixed(&root)).unwrap());
    assert_eq!(
        connection_file::read(fixed(&root)).unwrap(),
        bound.connection_info
    );
}

#[tokio::test]
async fn windows_copy_is_not_written_with_explicit_runtime_directory() {
    let root = ScratchDir::new("windows-copy-xdg");
    let config = config(&root, Some(root.join("runtime").into_os_string()));
    let primary = config.connection_file_path.clone();
    let bound = bound(ensure_singleton_with_config(config).await.unwrap());
    assert!(primary.exists());
    assert!(bound.connection_file_copy.is_none());
    assert!(!fixed(&root).exists());
}

#[tokio::test]
async fn windows_copy_and_primary_are_reclaimed_before_restart_publication() {
    let root = ScratchDir::new("windows-copy-stale");
    let config = config(&root, None);
    let primary = config.connection_file_path.clone();
    let first = bound(ensure_singleton_with_config(config.clone()).await.unwrap());
    let old_id = first.connection_info.daemon_id;
    drop(first);
    // Dropping the bound daemon closes its listeners but leaves both files; the
    // next startup removes both stale files before publishing its new identity.
    assert!(primary.exists());
    assert!(fixed(&root).exists());
    let next = bound(ensure_singleton_inner(config, false).await.unwrap());
    assert!(!primary.exists());
    assert!(!fixed(&root).exists());
    publish(&next).await;
    assert_ne!(next.connection_info.daemon_id, old_id);
    assert_eq!(fs::read(primary).unwrap(), fs::read(fixed(&root)).unwrap());
}

#[tokio::test]
async fn windows_copy_live_foreign_daemon_is_not_reclaimed_or_overwritten() {
    let root = ScratchDir::new("windows-copy-live");
    let first_config = config(&root, None);
    let first = bound(
        ensure_singleton_with_config(first_config.clone())
            .await
            .unwrap(),
    );
    let original = fs::read(fixed(&root)).unwrap();
    let server = serve(first);
    let mut second_config = first_config;
    second_config.connection_file_path = root.join("other-primary.json");
    let second = bound(ensure_singleton_with_config(second_config).await.unwrap());
    assert!(second.connection_file_copy.is_none());
    assert_eq!(fs::read(fixed(&root)).unwrap(), original);
    assert!(second.connection_file_path.exists());
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn windows_copy_publication_rechecks_a_live_foreign_daemon() {
    let root = ScratchDir::new("windows-copy-publish-race");
    let waiting = bound(
        ensure_singleton_inner(config(&root, None), false)
            .await
            .unwrap(),
    );
    let foreign_config = BootstrapConfig::new(root.join("foreign.json"), 0);
    let foreign = bound(ensure_singleton_with_config(foreign_config).await.unwrap());
    write_atomic_with_local_endpoint(fixed(&root), &foreign.connection_info, None).unwrap();
    let original = fs::read(fixed(&root)).unwrap();
    let server = serve(foreign);
    publish(&waiting).await;
    assert!(waiting.connection_file_path.exists());
    assert_eq!(fs::read(fixed(&root)).unwrap(), original);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn windows_copy_can_republish_its_own_live_identity() {
    let root = ScratchDir::new("windows-copy-own-identity");
    let daemon = bound(
        ensure_singleton_with_config(config(&root, None))
            .await
            .unwrap(),
    );
    let info = daemon.connection_info.clone();
    let primary = daemon.connection_file_path.clone();
    let server = serve(daemon);
    let copy = fixed(&root);
    let (result, _) = publish_connection_file(&primary, Some(&copy), &info, None, ()).await;
    result.unwrap();
    assert_eq!(fs::read(primary).unwrap(), fs::read(copy).unwrap());
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn windows_copy_failure_does_not_prevent_primary_publication() {
    let root = ScratchDir::new("windows-copy-failure");
    let waiting = bound(
        ensure_singleton_inner(config(&root, None), false)
            .await
            .unwrap(),
    );
    fs::create_dir(fixed(&root)).unwrap();
    publish(&waiting).await;
    assert_eq!(
        connection_file::read(&waiting.connection_file_path).unwrap(),
        waiting.connection_info
    );
    assert!(fixed(&root).is_dir());
}
