use super::*;
use crate::{router::RouteCtx, ControlHandler, FrameSink};
use subc_control::config::{ConfigControlResponse, ConfigThrough};
use subc_os::local_peer::{ObservedPeer, PeerObservation, PeerUser};
use subc_protocol::{Frame, FrameType};

async fn show(
    rig: &CaptureRig,
    project: &str,
    through: Option<ConfigThrough>,
) -> Result<serde_json::Value> {
    let response = rig
        .owner
        .settings_show_project(BTreeSet::new(), "a".into(), project.into(), through)
        .await?;
    Ok(serde_json::to_value(response).unwrap())
}

#[tokio::test]
async fn project_settings_stamped_show_retains_cutoffs_for_a_dropped_project() {
    let rig = CaptureRig::new();
    let private = root(&rig.project_scratch, "private");
    write_layer(&rig.workspace, 4);
    write_layer(&private, 9);
    std::fs::write(&rig.rig.declaration.user_file, br#"{"limit":2}"#).unwrap();
    rig.set_projects(vec![
        member("A", Some("W"), &[&private]),
        member("B", Some("W"), &[&rig.workspace]),
        member("C", None, &[]),
    ]);
    let plan = finish(&rig.owner, rig.prepare().await.unwrap());
    assert_eq!(
        show(&rig, "A", None).await.unwrap()["pending_plan"],
        serde_json::to_value(&plan.digest).unwrap()
    );
    let g1 = rig.apply(&plan).await.unwrap();
    rig.owner
        .lock()
        .unwrap()
        .store
        .record_delivery(&tuple(None), g1, true)
        .unwrap();
    rig.ack("A", g1);
    rig.ack("B", g1);

    let full = show(&rig, "A", None).await.unwrap();
    assert_eq!(full["effective"]["limit"], 9);
    assert_eq!(full["sources"]["/limit"], "project");
    assert_eq!(
        full["layers"],
        json!([
            {"layer":"user", "document":{"limit":2}},
            {"layer":"workspace", "workspace_id":"W", "document":{"limit":4}},
            {"layer":"project", "project_id":"A", "document":{"limit":9}}
        ])
    );
    let same_root = show(&rig, "B", None).await.unwrap();
    assert_eq!(same_root["layers"].as_array().unwrap().len(), 2);
    assert_eq!(same_root["layers"][1]["layer"], "project");
    assert_eq!(
        show(&rig, "B", Some(ConfigThrough::Workspace))
            .await
            .unwrap()["effective"]["limit"],
        2
    );
    assert_eq!(
        show(&rig, "C", None).await.unwrap()["effective"]["limit"],
        2
    );
    assert_eq!(
        show(&rig, "unknown", None).await.unwrap_err().code,
        "unknown_project"
    );

    rig.set_projects(vec![
        member("B", Some("W"), &[&rig.workspace]),
        member("C", None, &[]),
    ]);
    std::fs::write(&rig.rig.declaration.user_file, br#"{"limit":3}"#).unwrap();
    let plan = finish(&rig.owner, rig.prepare().await.unwrap());
    let g2 = rig.apply(&plan).await.unwrap();
    *rig.registry.reply.lock().unwrap() = None;
    let enumerations = rig.registry.enumerations.load(Ordering::Relaxed);
    for (cutoff, value, source, layers) in [
        (Some(ConfigThrough::Default), 1, "default", 0),
        (Some(ConfigThrough::User), 2, "user", 1),
        (Some(ConfigThrough::Workspace), 4, "workspace", 2),
        (None, 9, "project", 3),
    ] {
        let response = show(&rig, "A", cutoff).await.unwrap();
        assert_eq!(response["base_generation"], g2);
        assert_eq!(response["effective"]["limit"], value);
        assert_eq!(response["sources"]["/limit"], source);
        assert_eq!(response["layers"].as_array().unwrap().len(), layers);
    }
    assert_eq!(
        rig.registry.enumerations.load(Ordering::Relaxed),
        enumerations
    );
    assert_eq!(
        show(&rig, "C", None).await.unwrap_err().code,
        "project_registry_unavailable"
    );
}

#[tokio::test]
async fn project_settings_unstamped_show_reads_cutoffs_fresh() {
    let rig = CaptureRig::with_state(true, RootIoPool::new().unwrap(), false);
    let private = root(&rig.project_scratch, "private");
    let workspace = write_layer(&rig.workspace, 4);
    write_layer(&private, 9);
    rig.set_projects(vec![member("A", Some("W"), &[&private])]);
    assert_eq!(
        show(&rig, "A", Some(ConfigThrough::Workspace))
            .await
            .unwrap()["effective"]["limit"],
        4
    );
    std::fs::write(&workspace, br#"{"limit":5}"#).unwrap();
    let latest = show(&rig, "A", Some(ConfigThrough::Workspace))
        .await
        .unwrap();
    assert_eq!(latest["effective"]["limit"], 5);
    assert_eq!(latest["sources"]["/limit"], "workspace");
    assert_eq!(latest["base_generation"], 0);
    assert_eq!(latest["stamped"], false);
    assert_eq!(
        show(&rig, "A", None).await.unwrap()["effective"]["limit"],
        9
    );
}

fn owner_connection(handler: &ControlHandler, connection: ConnectionId) {
    #[cfg(unix)]
    let user = PeerUser::Uid(subc_os::effective_uid());
    #[cfg(windows)]
    let user = PeerUser::Sid(subc_os::named_pipe::current_user_sid_string().unwrap());
    handler.record_observed_peer(
        connection,
        ObservedPeer::Observed(PeerObservation {
            user,
            pid: std::process::id(),
            exe: None,
            signing_id: None,
        }),
    );
}

async fn request(
    handler: &ControlHandler,
    ctx: &RouteCtx,
    corr: u64,
    body: serde_json::Value,
) -> Vec<Frame> {
    handler
        .handle_control_frame(
            ctx,
            Frame::build(
                FrameType::Request,
                crate::control::control_flags(),
                0,
                0,
                corr,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap()
}

async fn event(
    rx: &mut tokio::sync::mpsc::Receiver<crate::router::OutboundFrame>,
) -> serde_json::Value {
    let frame = tokio::time::timeout(Duration::from_secs(7), rx.recv())
        .await
        .unwrap()
        .unwrap()
        .frame;
    assert_eq!(frame.header.ty, FrameType::StreamData);
    serde_json::from_slice(&frame.body).unwrap()
}

#[tokio::test]
async fn project_settings_watch_projects_isolate_validation_and_resync_on_drop() {
    let rig = CaptureRig::with_state(true, RootIoPool::new().unwrap(), false);
    let private = root(&rig.project_scratch, "private");
    let workspace = write_layer(&rig.workspace, 4);
    write_layer(&private, 6);
    rig.set_projects(vec![
        member("B", Some("W"), &[&private]),
        member("C", None, &[]),
        member("A", Some("W"), &[]),
    ]);
    let mut get = ConfigGetRequest::new();
    get.project_id = Some("C".into());
    let read = rig
        .owner
        .begin_unstamped_project_get(ConnectionId::new(10), "a".into(), get)
        .unwrap()
        .unwrap();
    assert!(rig
        .owner
        .read_unstamped_project(read)
        .await
        .unwrap()
        .active
        .unwrap()
        .attempt
        .is_none());

    let handler = ControlHandler::new(Arc::new(crate::Registry::default()))
        .with_config_runtime(rig.owner.clone());
    let connect = |id| {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        (
            RouteCtx {
                connection_id: ConnectionId::new(id),
                egress: FrameSink::new(tx),
            },
            rx,
        )
    };
    let (legacy, mut legacy_rx) = connect(76);
    let (opted, mut opted_rx) = connect(77);
    owner_connection(&handler, legacy.connection_id);
    let refused = request(
        &handler,
        &opted,
        2,
        json!({"op":"config.watch", "projects":true}),
    )
    .await;
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&refused[0].body).unwrap()["code"],
        "owner_required"
    );
    owner_connection(&handler, opted.connection_id);
    let described = request(&handler, &opted, 3, json!({"op":"server.describe"})).await;
    let described: serde_json::Value = serde_json::from_slice(&described[0].body).unwrap();
    assert!(described["capabilities"]
        .as_array()
        .unwrap()
        .contains(&json!("config.settings.projects/v1")));

    assert!(request(&handler, &legacy, 4, json!({"op":"config.watch"}))
        .await
        .is_empty());
    let user_only = event(&mut legacy_rx).await;
    assert!(request(
        &handler,
        &opted,
        5,
        json!({"op":"config.watch", "projects":true})
    )
    .await
    .is_empty());
    let initial = event(&mut opted_rx).await;
    assert_eq!(initial["event"], "resync");
    let rows = initial["modules"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(user_only["modules"], json!([rows[0]]));
    assert_eq!(rows[1]["project_id"], "A");
    assert_eq!(rows[2]["project_id"], "B");
    assert_eq!(rows[1]["workspace_id"], "W");
    assert_eq!(rows[2]["workspace_id"], "W");
    let modules = request(&handler, &opted, 6, json!({"op":"config.modules"})).await;
    let ConfigControlResponse::Modules { modules, .. } =
        serde_json::from_slice(&modules[0].body).unwrap()
    else {
        panic!("expected config.modules response")
    };
    assert_eq!(serde_json::to_value(modules).unwrap(), user_only["modules"]);

    // The limit may only increase across layers. Raising W's limit to 7
    // invalidates B's override of 6, but A inherits 7 and remains valid.
    std::fs::write(&workspace, br#"{"limit":7}"#).unwrap();
    let invalid_b = event(&mut opted_rx).await;
    assert_eq!(invalid_b["event"], "change");
    assert_eq!(invalid_b["project_id"], "B");
    assert_eq!(invalid_b["workspace_id"], "W");
    assert!(invalid_b["file_error"].is_object());
    assert_eq!(invalid_b["status"], rows[2]["status"]);
    assert!(legacy_rx.try_recv().is_err());

    std::fs::write(&workspace, b"not jsonc").unwrap();
    for project in ["A", "B"] {
        let invalid_file = event(&mut opted_rx).await;
        assert_eq!(invalid_file["event"], "change");
        assert_eq!(invalid_file["project_id"], project);
        assert_eq!(invalid_file["file_error"]["code"], "value_invalid");
    }
    assert!(legacy_rx.try_recv().is_err());
    assert!(opted_rx.try_recv().is_err());

    rig.set_projects(vec![member("A", Some("W"), &[]), member("C", None, &[])]);
    let dropped = event(&mut opted_rx).await;
    assert_eq!(dropped["event"], "resync");
    assert_eq!(dropped["modules"].as_array().unwrap().len(), 2);
    assert_eq!(dropped["modules"][1]["project_id"], "A");
    assert!(legacy_rx.try_recv().is_err());
    handler.cleanup_connection(opted.connection_id).unwrap();
    handler.cleanup_connection(legacy.connection_id).unwrap();
}

#[tokio::test]
async fn project_settings_stamped_proposal_and_activation_leave_legacy_reads_unchanged() {
    let rig = CaptureRig::new();
    let private = root(&rig.project_scratch, "private");
    rig.set_projects(vec![member("A", None, &[&private])]);
    rig.owner.refresh_project_membership().await.unwrap();
    let cutoffs = [
        None,
        Some(ConfigThrough::Default),
        Some(ConfigThrough::User),
        Some(ConfigThrough::Workspace),
    ];
    let legacy_show = |through| {
        serde_json::to_vec(
            &rig.owner
                .settings_show(BTreeSet::new(), "a".into(), through)
                .unwrap(),
        )
        .unwrap()
    };
    let legacy_modules =
        || serde_json::to_vec(&rig.owner.settings_modules(BTreeSet::new()).unwrap()).unwrap();
    let before = cutoffs.map(legacy_show);
    let legacy = legacy_modules();
    let path = write_layer(&private, 9);
    rig.owner.refresh_settings_projects(true).await.unwrap();
    let rows = || {
        let response = rig
            .owner
            .observe_settings_rows(BTreeSet::new(), true, true)
            .unwrap()
            .unwrap()
            .response;
        let ConfigControlResponse::Modules { modules, .. } = response else {
            panic!("expected modules")
        };
        modules
    };
    assert_eq!(rows()[1].status, "pending_proposal");
    let plan = finish(&rig.owner, rig.prepare().await.unwrap());
    assert_eq!(
        show(&rig, "A", None).await.unwrap()["pending_plan"],
        serde_json::to_value(&plan.digest).unwrap()
    );
    assert_eq!(cutoffs.map(legacy_show), before);
    assert_eq!(legacy_modules(), legacy);
    let generation = rig.apply(&plan).await.unwrap();
    assert_eq!(legacy_modules(), legacy);
    let pending = rows();
    assert_eq!(pending[1].status, "pending_activation");
    assert_eq!(pending[1].approved_generation, Some(generation));
    assert_eq!(pending[1].active_generation, None);
    std::fs::write(&path, b"not jsonc").unwrap();
    rig.owner.refresh_settings_projects(true).await.unwrap();
    let invalid = rows();
    assert_eq!(invalid[1].status, "pending_activation");
    assert_eq!(
        invalid[1].file_error.as_ref().unwrap().code,
        "value_invalid"
    );
    let shown = show(&rig, "A", None).await.unwrap();
    assert_eq!(shown["file_error"]["code"], "value_invalid");
    assert_eq!(shown["effective"]["limit"], 9);
    assert_eq!(legacy_modules(), legacy);
    for (through, before) in cutoffs.into_iter().zip(before) {
        let mut expected: serde_json::Value = serde_json::from_slice(&before).unwrap();
        expected["base_generation"] = generation.into();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&legacy_show(through)).unwrap(),
            expected
        );
    }
}
