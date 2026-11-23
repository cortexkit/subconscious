use super::*;
use crate::{control::control_flags, router::RouteCtx, ControlHandler, ForwardingTable, FrameSink};
use subc_protocol::{
    config::ConfigGetReply,
    manifest::{Concurrency, ModuleManifest},
    Frame, FrameType, PROTOCOL_VERSION,
};
use tokio::{
    sync::{mpsc, oneshot},
    time::timeout,
};

const CONNECTION: ConnectionId = ConnectionId::new(10);

fn unstamped() -> CaptureRig {
    CaptureRig::with_state(true, RootIoPool::new().unwrap(), false)
}

fn request(project: Option<&str>) -> ConfigGetRequest {
    let mut request = ConfigGetRequest::new();
    request.project_id = project.map(str::to_owned);
    request
}

async fn get(rig: &CaptureRig, connection: ConnectionId, id: &str) -> Result<ConfigView> {
    let read = rig
        .owner
        .begin_unstamped_project_get(connection, "a".into(), request(Some(id)))?
        .unwrap();
    let reply = rig.owner.read_unstamped_project(read).await?;
    assert!(reply.pending.is_none());
    let view = reply.active.unwrap();
    assert_eq!(view.project_id.as_deref(), Some(id));
    assert_eq!(view.generation, 0);
    assert_eq!(view.status, ConfigStatus::Unstamped);
    assert!(view.attempt.is_none());
    Ok(view)
}

async fn poll(rig: &CaptureRig) -> Vec<String> {
    let (sender, mut hints) = mpsc::unbounded_channel();
    rig.owner.poll_unstamped_projects(sender).await.unwrap();
    let mut ids = Vec::new();
    while let Ok(hint) = hints.try_recv() {
        assert_eq!(hint.hint.generation, 0);
        assert_eq!(hint.connection, CONNECTION);
        assert!(rig.owner.unstamped_project_hint_current(&hint).unwrap());
        ids.push(hint.hint.project_id.unwrap());
    }
    ids.sort();
    ids
}

#[tokio::test]
async fn unstamped_poll_shares_observation_between_equal_declarations() {
    let rig = unstamped();
    let project = root(&rig.project_scratch, "private");
    rig.set_projects(vec![member("P", None, &[&project])]);
    let other = ConnectionId::new(11);
    rig.owner
        .declare(other, "a", rig.rig.declaration.clone(), false)
        .unwrap();
    for connection in [CONNECTION, other] {
        get(&rig, connection, "P").await.unwrap();
    }

    write_layer(&project, 9);
    let (before, rebuilds) = rig.owner.live_project_counts();
    let (sender, mut hints) = mpsc::unbounded_channel();
    rig.owner.poll_unstamped_projects(sender).await.unwrap();
    let (after, after_rebuilds) = rig.owner.live_project_counts();
    assert_eq!(after - before, 1, "one observation for two connections");
    assert_eq!(after_rebuilds - rebuilds, 1);
    let mut recipients = std::collections::HashSet::new();
    while let Ok(hint) = hints.try_recv() {
        assert_eq!(hint.hint.project_id.as_deref(), Some("P"));
        assert!(rig.owner.unstamped_project_hint_current(&hint).unwrap());
        assert!(recipients.insert(hint.connection));
    }
    assert_eq!(
        recipients,
        std::collections::HashSet::from([CONNECTION, other])
    );

    let different = ConnectionId::new(12);
    let mut declaration = rig.rig.declaration.clone();
    declaration.schema["properties"]["limit"]["maximum"] = json!(9);
    rig.owner
        .declare(different, "a", declaration, false)
        .unwrap();
    get(&rig, different, "P").await.unwrap();
    write_layer(&project, 10);
    let (sender, mut hints) = mpsc::unbounded_channel();
    rig.owner.poll_unstamped_projects(sender).await.unwrap();
    let mut recipients = std::collections::HashSet::new();
    while let Ok(hint) = hints.try_recv() {
        assert!(recipients.insert(hint.connection));
    }
    assert_eq!(
        recipients,
        std::collections::HashSet::from([CONNECTION, other])
    );
}

#[tokio::test]
async fn unstamped_poll_skips_unchanged_view_and_records_layer_edits() {
    let rig = unstamped();
    let project = root(&rig.project_scratch, "private");
    rig.set_projects(vec![member("P", None, &[&project])]);
    let layer = write_layer(&project, 9);
    get(&rig, CONNECTION, "P").await.unwrap();
    let rebuilds = || rig.owner.live_project_counts().1;
    let before = rebuilds();
    let row = || {
        rig.owner
            .unstamped_project_observations()
            .unwrap()
            .remove(&("a".into(), "P".into()))
            .unwrap()
    };
    let changes = rig.owner.subscribe_settings();

    assert!(poll(&rig).await.is_empty());
    assert_eq!(rebuilds() - before, 0, "unchanged view was rebuilt");
    assert!(!changes.has_changed().unwrap());

    std::fs::write(&layer, b"{\"limit\":10}").unwrap();
    assert_eq!(poll(&rig).await, ["P"]);
    assert_eq!(rebuilds() - before, 1);

    std::fs::write(&layer, b"not jsonc").unwrap();
    assert!(poll(&rig).await.is_empty());
    assert_eq!(rebuilds() - before, 2);
    assert!(row().file_error.is_some());

    std::fs::write(&layer, b"{\"limit\":10}").unwrap();
    assert_eq!(poll(&rig).await, ["P"]);
    assert_eq!(rebuilds() - before, 3);
    assert!(row().file_error.is_none());

    let daemon = rig.rig.path.join("subc.jsonc");
    let mut bytes = std::fs::read(&daemon).unwrap();
    bytes.extend_from_slice(b"\n// daemon-only edit\n");
    std::fs::write(&daemon, bytes).unwrap();
    assert!(poll(&rig).await.is_empty());
    assert_eq!(rebuilds() - before, 4);

    std::fs::write(&daemon, b"not jsonc").unwrap();
    assert!(poll(&rig).await.is_empty());
    assert_eq!(row().file_error.unwrap().code, "invalid_daemon_config");
}

#[tokio::test]
async fn unstamped_projects_poll_first_file_user_change_drop_and_registration() {
    let rig = unstamped();
    let project = root(&rig.project_scratch, "private");
    let registered = member("P", Some("W"), &[&project]);
    rig.set_projects(vec![registered.clone()]);
    let fallback = get(&rig, CONNECTION, "P").await.unwrap();
    assert_eq!(fallback.effective["limit"], 1);
    assert_eq!(fallback.layers.len(), 1);
    assert!(fallback.layers[0].project_id.is_none());
    let mut changes = rig.owner.subscribe_settings();

    let file = write_layer(&project, 9);
    assert_eq!(poll(&rig).await, ["P"]);
    assert!(changes.has_changed().unwrap());
    changes.borrow_and_update();
    let view = get(&rig, CONNECTION, "P").await.unwrap();
    assert_eq!(view.effective["limit"], 9);
    assert_eq!(view.layers[1].layer, ConfigLayer::Project);
    std::fs::write(&file, b"{\"limit\":10}").unwrap();
    assert_eq!(
        get(&rig, CONNECTION, "P").await.unwrap().effective["limit"],
        10,
        "get does not wait for a poll"
    );
    std::fs::remove_file(&file).unwrap();
    assert_eq!(poll(&rig).await, ["P"]);
    assert!(
        !rig.owner.unstamped_project_observations().unwrap()[&("a".into(), "P".into())].present
    );

    std::fs::write(&rig.rig.declaration.user_file, b"{\"limit\":3}").unwrap();
    assert_eq!(poll(&rig).await, ["P"]);
    assert_eq!(
        get(&rig, CONNECTION, "P").await.unwrap().effective["limit"],
        3
    );

    changes.borrow_and_update();
    *rig.registry.reply.lock().unwrap() = None;
    let (sender, mut hints) = mpsc::unbounded_channel();
    assert_eq!(
        rig.owner
            .poll_unstamped_projects(sender)
            .await
            .unwrap_err()
            .code,
        "project_registry_unavailable"
    );
    assert!(hints.try_recv().is_err());
    assert_eq!(
        get(&rig, CONNECTION, "P").await.unwrap_err().code,
        "project_registry_unavailable"
    );
    assert!(!changes.has_changed().unwrap());
    rig.set_projects(Vec::new());
    assert!(poll(&rig).await.is_empty());
    assert!(changes.has_changed().unwrap());
    assert!(rig
        .owner
        .unstamped_project_observations()
        .unwrap()
        .is_empty());
    assert_eq!(
        get(&rig, CONNECTION, "P").await.unwrap_err().code,
        "unknown_project"
    );
    rig.set_projects(vec![registered]);
    assert_eq!(poll(&rig).await, ["P"]);
}

#[tokio::test]
async fn unstamped_project_views_never_read_the_module_program() {
    // The poll re-reads every requested project every two seconds. A view
    // needs only settings files, so it must not open the module's program:
    // hashing the program on every poll kept the daemon at about two cores.
    let rig = unstamped();
    let project = root(&rig.project_scratch, "private");
    rig.set_projects(vec![member("P", Some("W"), &[&project])]);
    get(&rig, CONNECTION, "P").await.unwrap();
    std::fs::remove_file(rig.rig.path.join("program")).unwrap();

    write_layer(&project, 9);
    assert_eq!(poll(&rig).await, ["P"]);
    assert_eq!(
        get(&rig, CONNECTION, "P").await.unwrap().effective["limit"],
        9
    );
}

#[tokio::test]
async fn unstamped_projects_poll_isolates_shared_file_refusal_and_settings_wakes() {
    let rig = unstamped();
    let b = root(&rig.project_scratch, "b");
    rig.set_projects(vec![
        member("A", Some("W"), &[]),
        member("B", Some("W"), &[&b]),
    ]);
    let workspace = write_layer(&rig.workspace, 4);
    write_layer(&b, 6);
    assert_eq!(
        get(&rig, CONNECTION, "A").await.unwrap().effective["limit"],
        4
    );
    assert_eq!(
        get(&rig, CONNECTION, "B").await.unwrap().effective["limit"],
        6
    );
    let enumerations = rig.registry.enumerations.load(Ordering::Relaxed);
    let mut changes = rig.owner.subscribe_settings();
    std::fs::write(&workspace, b"{\"limit\":7}").unwrap();
    assert_eq!(poll(&rig).await, ["A"]);
    assert_eq!(
        rig.registry.enumerations.load(Ordering::Relaxed) - enumerations,
        1
    );
    assert!(changes.has_changed().unwrap());
    changes.borrow_and_update();
    let observations = || rig.owner.unstamped_project_observations().unwrap();
    let key = |p: &str| ("a".to_owned(), p.into());
    let rows = observations();
    assert_eq!(rows[&key("A")].workspace_id.as_deref(), Some("W"));
    assert!(rows[&key("A")].file_error.is_none());
    assert!(rows[&key("B")].file_error.is_some());
    assert!(get(&rig, CONNECTION, "B").await.is_err());
    assert_eq!(
        get(&rig, CONNECTION, "A").await.unwrap().effective["limit"],
        7
    );
    let user = rig
        .owner
        .module_get(CONNECTION, "a", request(None))
        .unwrap()
        .active
        .unwrap();
    assert_eq!(user.effective["limit"], 1);
    assert!(!changes.has_changed().unwrap());
    std::fs::write(workspace, b"not jsonc").unwrap();
    assert!(poll(&rig).await.is_empty());
    assert!(changes.has_changed().unwrap());
    assert!(observations().values().all(|row| row.file_error.is_some()));
}

struct Wire {
    handler: Arc<ControlHandler>,
    context: RouteCtx,
    outbound: mpsc::Receiver<crate::router::OutboundFrame>,
}

impl Wire {
    fn new(rig: &CaptureRig) -> Self {
        let registry = Arc::new(crate::Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        registry
            .register_with_control_ops(
                ModuleManifest::builder("a", "0.1.0").build(),
                PROTOCOL_VERSION,
                CONNECTION,
                vec![],
            )
            .unwrap();
        let (sender, outbound) = mpsc::channel(8);
        let egress = FrameSink::new(sender);
        forwarding
            .register_module_connection(
                CONNECTION,
                "a".into(),
                PROTOCOL_VERSION,
                Concurrency::ModuleManaged,
                egress.clone(),
            )
            .unwrap();
        Self {
            handler: Arc::new(
                ControlHandler::with_forwarding(registry, forwarding)
                    .with_config_runtime(rig.owner.clone()),
            ),
            context: RouteCtx {
                connection_id: CONNECTION,
                egress,
            },
            outbound,
        }
    }

    async fn get(&mut self, project: Option<&str>) -> ConfigGetReply {
        let request = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            1,
            serde_json::to_vec(&request(project)).unwrap(),
        )
        .unwrap();
        let handler = self.handler.clone();
        let ctx = &self.context;
        let run = handler.handle_control_frame(ctx, request);
        let reply = async {
            let mut frame = self.outbound.recv().await.unwrap();
            assert_eq!(frame.frame.header.ty, FrameType::Response);
            frame.flushed.take().unwrap().send(()).unwrap();
            serde_json::from_slice(&frame.frame.body).unwrap()
        };
        // A refusal returns a frame directly, rather than waiting for a flush.
        tokio::pin!(run, reply);
        tokio::select! {
            response = &mut run => panic!("get refused: {response:?}"),
            response = &mut reply => {
                assert!(run.await.unwrap().is_empty());
                response
            }
        }
    }
}

#[tokio::test]
async fn unstamped_project_get_waits_for_delivery_poll_on_the_same_root() {
    let rig = unstamped();
    let project = root(&rig.project_scratch, "project");
    rig.set_projects(vec![member("P", None, &[&project])]);
    let mut wire = Wire::new(&rig);
    let fallback = wire.get(Some("P")).await.active.unwrap();
    assert_eq!(fallback.layers.len(), 1);

    let (entered, polling_root) = oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let block = Mutex::new(Some((entered, blocked)));
    *rig.registry.pool.before_work.lock().unwrap() = Some(Arc::new(move |_| {
        if let Some((entered, blocked)) = block.lock().unwrap().take() {
            let _ = entered.send(());
            let _ = blocked.recv();
        }
    }));
    let (hints, _recv_hints) = mpsc::unbounded_channel();
    let poll = rig.owner.poll_unstamped_projects(hints);
    let get_during_poll = async {
        polling_root.await.unwrap();
        write_layer(&project, 9);
        let (signal, snapshot_polled) = oneshot::channel();
        *rig.registry.after_snapshot_poll.lock().unwrap() = Some(signal);
        let (reply, ()) = tokio::join!(wire.get(Some("P")), async {
            // The get must attempt the capture while the poll holds the root.
            snapshot_polled.await.unwrap();
            release.send(()).unwrap();
        });
        let view = reply.active.unwrap();
        assert_eq!(view.project_id.as_deref(), Some("P"));
        assert_eq!(view.status, ConfigStatus::Unstamped);
        assert_eq!(view.effective["limit"], 9);
    };
    timeout(std::time::Duration::from_secs(5), async {
        let (result, ()) = tokio::join!(poll, get_during_poll);
        result.unwrap();
    })
    .await
    .expect("the get should return after the poll releases the root");
}

#[tokio::test]
async fn unstamped_projects_stuck_root_does_not_delay_wire_push_or_bare_get() {
    use std::time::Duration;
    let rig = unstamped();
    let b = root(&rig.project_scratch, "b");
    rig.set_projects(vec![
        member("A", Some("W"), &[]),
        member("B", Some("W"), &[&b]),
    ]);
    let workspace = write_layer(&rig.workspace, 4);
    let mut wire = Wire::new(&rig);
    let root_reads = Arc::new(AtomicUsize::new(0));
    *rig.registry.pool.before_work.lock().unwrap() = Some(Arc::new({
        let root_reads = root_reads.clone();
        move |_| {
            root_reads.fetch_add(1, Ordering::Relaxed);
        }
    }));
    let bare = wire.get(None).await.active.unwrap();
    assert_eq!(bare.effective["limit"], 1);
    assert_eq!(root_reads.load(Ordering::Relaxed), 0);
    assert_eq!(rig.registry.enumerations.load(Ordering::Relaxed), 0);
    assert_eq!(
        wire.get(Some("A")).await.active.unwrap().effective["limit"],
        4
    );
    assert_eq!(
        wire.get(Some("B")).await.active.unwrap().effective["limit"],
        4
    );
    let (entered, stuck) = oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let block = Mutex::new(Some((entered, blocked)));
    *rig.registry.pool.before_work.lock().unwrap() = Some(Arc::new(move |path| {
        if path == b {
            if let Some((entered, blocked)) = block.lock().unwrap().take() {
                let _ = entered.send(());
                let _ = blocked.recv();
            }
        }
    }));
    std::fs::write(workspace, b"{\"limit\":7}").unwrap();
    let before_tick = rig.registry.enumerations.load(Ordering::Relaxed);
    let worker = wire.handler.spawn_config_delivery_worker().unwrap();
    timeout(Duration::from_secs(2), async {
        stuck.await.unwrap();
        let mut frame = wire.outbound.recv().await.unwrap();
        assert_eq!(frame.frame.header.ty, FrameType::Push);
        let hint: serde_json::Value = serde_json::from_slice(&frame.frame.body).unwrap();
        assert_eq!(
            hint,
            json!({"op":"config.changed", "generation":0, "project_id":"A", "fields":[]})
        );
        frame.flushed.take().unwrap().send(()).unwrap();
        assert_eq!(
            rig.registry.enumerations.load(Ordering::Relaxed) - before_tick,
            1
        );
        assert_eq!(wire.get(None).await.active.unwrap().effective["limit"], 1);
        assert_eq!(
            wire.get(Some("A")).await.active.unwrap().effective["limit"],
            7
        );
    })
    .await
    .expect("the first tick must push A while B is still blocked");
    release.send(()).unwrap();
    worker.abort();
}

#[tokio::test]
async fn unstamped_projects_present_files_cannot_bypass_validation() {
    let rig = unstamped();
    let p = root(&rig.project_scratch, "p");
    rig.set_projects(vec![member("P", None, &[&p])]);
    let path = write_layer(&p, 6);
    let alternate = ConnectionId::new(11);
    let mut declaration = rig.rig.declaration.clone();
    declaration.schema["properties"]["limit"]["x-ck-apply"] = json!("route_rebind");
    declaration.schema["properties"]["section"] = json!({
        "type":"object", "unevaluatedProperties":false, "properties": {
            "secret": {"type":"object", "description":"Vault reference", "x-ck-kind":"credential_ref", "x-ck-layers":["user"]},
            "enabled": {"type":"boolean", "description":"Enabled", "default":false}
        }
    });
    rig.owner
        .declare(alternate, "a", declaration, true)
        .unwrap();
    assert_eq!(
        get(&rig, alternate, "P").await.unwrap_err().code,
        "project_apply_kind_unsupported"
    );
    std::fs::write(&path, br#"{"section":{"enabled":true}}"#).unwrap();
    assert_eq!(
        get(&rig, alternate, "P").await.unwrap_err().code,
        "credential_layer_unsupported"
    );
    std::fs::write(&path, br#"{"limit":"{env:sensitive-literal}"}"#).unwrap();
    let error = get(&rig, alternate, "P").await.unwrap_err();
    assert_eq!(error.code, "value_invalid");
    assert!(!error.detail.contains("sensitive-literal"));
}
