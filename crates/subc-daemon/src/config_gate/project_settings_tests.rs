use super::*;
use crate::router::test_log::{captured_logs, log_capture};
use crate::{router::RouteCtx, ControlHandler, FrameSink};
use subc_control::config::{ConfigControlResponse, ConfigThrough};
use subc_os::local_peer::{ObservedPeer, PeerObservation, PeerUser};
use subc_protocol::{Frame, FrameType};
use tokio::sync::mpsc::error::TryRecvError;

#[cfg(target_os = "linux")]
#[path = "project_vectors_tests.rs"]
mod project_vectors;

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
async fn project_settings_show_attributes_empty_objects_without_changing_approval() {
    let rig = CaptureRig::with_declaration(true, RootIoPool::new().unwrap(), true, |declaration| {
        declaration.schema["properties"]["projects"] = json!({
            "type":"object","unevaluatedProperties":false,"default":{},
            "patternProperties":{"^[a-z]+$":{
                "type":"boolean","description":"Enable this project.","default":false
            }}
        });
    });
    let private = root(&rig.project_scratch, "private");
    let project_file = write_layer(&private, 9);
    std::fs::write(project_file, br#"{"limit":9,"projects":{}}"#).unwrap();
    rig.set_projects(vec![member("A", None, &[&private])]);
    let plan = finish(&rig.owner, rig.prepare().await.unwrap());
    let generation = rig.apply(&plan).await.unwrap();

    let stored_digest = || {
        let state = rig.owner.lock().unwrap();
        let (_, body) = state.store.loaded_admission_state();
        let delivery = body.generations[&generation].delivery.as_ref().unwrap();
        let stored = &delivery.project_views["a"]["A"];
        assert_eq!(stored.effective["projects"], json!({}));
        assert_eq!(
            stored.sources,
            BTreeMap::from([("/limit".into(), subc_config::Layer::Project)])
        );
        serde_json::to_vec(&delivery.project_approval_digests["a"]["A"]).unwrap()
    };
    let before = stored_digest();
    for (cutoff, source) in [
        (Some(ConfigThrough::Default), "default"),
        (Some(ConfigThrough::User), "default"),
        (Some(ConfigThrough::Workspace), "default"),
        (None, "project"),
    ] {
        let reply = show(&rig, "A", cutoff).await.unwrap();
        assert_eq!(reply["effective"]["projects"], json!({}));
        assert_eq!(reply["sources"]["/projects"], source);
        assert!(reply["sources"].get("").is_none());
    }
    assert_eq!(stored_digest(), before);
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
    handler.record_local_connection(connection);
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

async fn event_frame(rx: &mut tokio::sync::mpsc::Receiver<crate::router::OutboundFrame>) -> Frame {
    let frame = tokio::time::timeout(Duration::from_secs(7), rx.recv())
        .await
        .unwrap()
        .unwrap()
        .frame;
    assert_eq!(frame.header.ty, FrameType::StreamData);
    frame
}

async fn event(
    rx: &mut tokio::sync::mpsc::Receiver<crate::router::OutboundFrame>,
) -> serde_json::Value {
    serde_json::from_slice(&event_frame(rx).await.body).unwrap()
}

fn status_handler(rig: &CaptureRig) -> ControlHandler {
    ControlHandler::new(Arc::new(crate::Registry::default())).with_config_runtime(rig.owner.clone())
}

async fn config_status(handler: &ControlHandler) -> serde_json::Value {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let ctx = RouteCtx {
        connection_id: ConnectionId::new(70),
        egress: FrameSink::new(tx),
    };
    owner_connection(handler, ctx.connection_id);
    let reply = tokio::time::timeout(
        Duration::from_secs(5),
        request(handler, &ctx, 1, json!({"op":"config.status"})),
    )
    .await
    .expect("status should not wait for the owner or a stalled root");
    assert_eq!(reply[0].header.ty, FrameType::Response);
    serde_json::from_slice(&reply[0].body).unwrap()
}

#[tokio::test]
async fn project_settings_status_reports_fresh_file_errors_without_owner_lock() {
    let rig = CaptureRig::with_state(true, RootIoPool::new().unwrap(), false);
    let private = root(&rig.project_scratch, "private");
    write_layer(&rig.workspace, 4);
    let file = write_layer(&private, 3);
    rig.set_projects(vec![member("A", Some("W"), &[&private])]);
    let handler = status_handler(&rig);

    let owner = rig.owner.clone();
    let (entered, held) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let _guard = owner.lock().unwrap();
        entered.send(()).unwrap();
        let _ = wait.recv();
    });
    held.await.unwrap();
    let invalid = config_status(&handler).await;
    drop(release);
    holder.join().unwrap();
    assert_eq!(invalid["status"]["state"], "unstamped");
    assert_eq!(
        invalid["status"]["file_errors"],
        json!([{
            "module_id":"a", "project_id":"A", "workspace_id":"W",
            "code":"bound_violated", "message":"a/A: /limit", "layer":"project", "path":file
        }])
    );
    assert!(invalid["status"].get("file_errors_incomplete").is_none());
    #[cfg(target_os = "linux")]
    project_vectors::assert_status_file_errors_sample(
        "project-status-file-errors.json",
        invalid,
        &rig,
    );

    std::fs::write(file, br#"{"limit":5}"#).unwrap();
    let valid = config_status(&handler).await;
    assert!(valid["status"].get("file_errors").is_none());
    assert!(valid["status"].get("file_errors_incomplete").is_none());
}

#[tokio::test]
async fn project_settings_status_attributes_forbidden_workspace_field() {
    let rig = CaptureRig::with_state(true, RootIoPool::new().unwrap(), false);
    let private = root(&rig.project_scratch, "private");
    write_layer(&private, 9);
    let workspace = write_layer(&rig.workspace, 4);
    std::fs::write(&workspace, br#"{"limit":4,"consumers":2}"#).unwrap();
    let mut declaration = rig.rig.declaration.clone();
    declaration.schema["properties"]["consumers"] = json!({
        "type":"integer", "description":"Consumer count", "default":1, "x-ck-layers":["user"]
    });
    rig.owner
        .declare(ConnectionId::new(11), "a", declaration, true)
        .unwrap();
    rig.set_projects(vec![
        member("A", Some("W"), &[&private]),
        member("B", Some("W"), &[]),
    ]);
    let invalid = config_status(&status_handler(&rig)).await;
    let expected = ["A", "B"].map(|project| {
        json!({
            "module_id":"a", "project_id":project, "workspace_id":"W",
            "code":"layer_forbidden", "message":format!("a/{project}: /consumers"),
            "layer":"workspace", "path":workspace
        })
    });
    assert_eq!(invalid["status"]["file_errors"], json!(expected));
    assert!(invalid["status"].get("file_errors_incomplete").is_none());
    #[cfg(target_os = "linux")]
    project_vectors::assert_status_file_errors_sample(
        "project-status-workspace-file-error.json",
        invalid,
        &rig,
    );
}

#[tokio::test]
async fn project_settings_status_attributes_a_refused_file_open() {
    let rig = CaptureRig::with_state(true, RootIoPool::new().unwrap(), false);
    let private = root(&rig.project_scratch, "private");
    let file = write_layer(&private, 4);
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    rig.set_projects(vec![member("A", None, &[&private])]);
    let invalid = config_status(&status_handler(&rig)).await;
    let error = &invalid["status"]["file_errors"][0];
    assert_eq!(error["code"], "layer_not_regular");
    assert_eq!(error["layer"], "project");
    assert_eq!(error["path"], json!(file));
}

#[tokio::test]
async fn project_settings_status_reports_user_errors_when_registry_is_down() {
    let rig = CaptureRig::with_state(true, RootIoPool::new().unwrap(), false);
    std::fs::write(&rig.rig.declaration.user_file, b"not jsonc").unwrap();
    let reply = config_status(&status_handler(&rig)).await;
    let status = &reply["status"];
    assert_eq!(
        status["file_errors_incomplete"],
        json!({"reason":"project_registry_unavailable"})
    );
    let errors = status["file_errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["module_id"], "a");
    assert_eq!(errors[0]["code"], "value_invalid");
    assert!(errors[0].get("project_id").is_none());
    assert!(errors[0].get("workspace_id").is_none());
    assert!(errors[0].get("layer").is_none());
    assert_eq!(errors[0]["path"], rig.rig.declaration.user_file);
}

#[tokio::test]
async fn project_settings_stamped_user_file_refusal_logs_across_status_and_watch() {
    let rig = CaptureRig::new();
    let owner = &rig.owner;
    let path = &rig.rig.declaration.user_file;
    let handler = status_handler(&rig);
    let watch_row = || {
        let observation = owner
            .observe_settings_rows(BTreeSet::new(), true, true)
            .unwrap()
            .unwrap();
        let ConfigControlResponse::Modules { modules, stamped } = observation.response else {
            panic!("expected settings rows");
        };
        assert!(stamped);
        modules
            .into_iter()
            .find(|row| row.module_id == "a" && row.project_id.is_none())
            .unwrap()
    };

    let (logs, _guard) = log_capture(tracing::Level::INFO);
    std::fs::write(path, br#"{"limit":3,"not/declared":true}"#).unwrap();
    let expected = json!({"code":"value_invalid", "message":"a: /not~1declared", "path":path});
    assert_eq!(
        serde_json::to_value(watch_row().file_error).unwrap(),
        expected
    );
    for _ in 0..3 {
        assert_eq!(
            serde_json::to_value(&owner.polled_status().unwrap().file_errors[0].error).unwrap(),
            expected
        );
    }
    let status = config_status(&handler).await;
    assert_eq!(status["status"]["state"], "stamped");
    assert_eq!(
        status["status"]["file_errors"],
        json!([{
            "module_id":"a", "code":"value_invalid", "message":"a: /not~1declared", "path":path
        }])
    );
    let text = captured_logs(&logs);
    assert_eq!(
        text.matches("user settings file refused").count(),
        1,
        "{text}"
    );
    let shown = owner
        .settings_show(BTreeSet::new(), "a".into(), None)
        .unwrap();
    assert_eq!(
        serde_json::to_value(shown).unwrap()["effective"]["limit"],
        1
    );

    std::fs::write(path, br#"{"limit":4,"not/declared":true}"#).unwrap();
    assert_eq!(owner.polled_status().unwrap().file_errors.len(), 1);
    assert!(watch_row().file_error.is_some());
    assert_eq!(
        captured_logs(&logs)
            .matches("user settings file refused")
            .count(),
        2
    );

    std::fs::write(path, br#"{"limit":4}"#).unwrap();
    assert!(watch_row().file_error.is_none());
    assert!(owner.polled_status().unwrap().file_errors.is_empty());
    assert_eq!(
        captured_logs(&logs)
            .matches("user settings file accepted after refusal")
            .count(),
        1
    );
    let shown = owner
        .settings_show(BTreeSet::new(), "a".into(), None)
        .unwrap();
    assert_eq!(
        serde_json::to_value(shown).unwrap()["effective"]["limit"],
        1
    );
}

#[tokio::test]
async fn project_settings_status_keeps_completed_rows_when_a_root_stalls() {
    let rig = CaptureRig::with_state(true, RootIoPool::new().unwrap(), false);
    let fast = root(&rig.project_scratch, "fast");
    let slow = root(&rig.project_scratch, "slow");
    write_layer(&rig.workspace, 4);
    let fast_file = write_layer(&fast, 3);
    write_layer(&slow, 3);
    rig.set_projects(vec![
        member("fast", Some("W"), &[&fast]),
        member("slow", Some("W"), &[&slow]),
    ]);
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let wait = std::sync::Mutex::new(wait);
    let (entered, held) = std::sync::mpsc::channel();
    *rig.registry.pool.before_work.lock().unwrap() = Some(Arc::new(move |path| {
        if path == slow {
            entered.send(()).unwrap();
            let _ = wait.lock().unwrap().recv();
        }
    }));
    let reply = config_status(&status_handler(&rig)).await;
    drop(release);
    assert!(
        held.try_recv().is_ok(),
        "slow root must have reached the gate"
    );
    let status = &reply["status"];
    assert_eq!(
        status["file_errors_incomplete"],
        json!({"reason":"config_unavailable"})
    );
    assert_eq!(
        status["file_errors"],
        json!([{
            "module_id":"a", "project_id":"fast", "workspace_id":"W",
            "code":"bound_violated", "message":"a/fast: /limit",
            "layer":"project", "path":fast_file
        }])
    );
}

#[tokio::test]
async fn project_settings_watch_projects_isolate_validation_and_resync_on_drop() {
    let rig = CaptureRig::with_state(true, RootIoPool::new().unwrap(), false);
    let private = root(&rig.project_scratch, "private");
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
    let (baseline, mut baseline_rx) = connect(78);
    let legacy_request = json!({"op":"config.watch"});
    owner_connection(&handler, baseline.connection_id);
    assert!(request(&handler, &baseline, 1, legacy_request.clone())
        .await
        .is_empty());
    let baseline_resync = event_frame(&mut baseline_rx).await;
    handler.cleanup_connection(baseline.connection_id).unwrap();
    let workspace = write_layer(&rig.workspace, 4);
    let project_file = write_layer(&private, 6);

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

    assert!(request(&handler, &legacy, 4, legacy_request)
        .await
        .is_empty());
    let legacy_resync = event_frame(&mut legacy_rx).await;
    assert_eq!(legacy_resync.body, baseline_resync.body);
    let user_only: serde_json::Value = serde_json::from_slice(&legacy_resync.body).unwrap();
    let watch = json!({"op":"config.watch", "projects":true});
    assert!(request(&handler, &opted, 5, watch.clone()).await.is_empty());
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
    assert_eq!(invalid_b["file_error"]["layer"], "project");
    assert_eq!(invalid_b["file_error"]["path"], json!(project_file));
    assert_eq!(invalid_b["status"], rows[2]["status"]);
    assert!(matches!(legacy_rx.try_recv(), Err(TryRecvError::Empty)));

    std::fs::write(&workspace, b"not jsonc").unwrap();
    for project in ["A", "B"] {
        let invalid_file = event(&mut opted_rx).await;
        assert_eq!(invalid_file["event"], "change");
        assert_eq!(invalid_file["project_id"], project);
        assert_eq!(invalid_file["file_error"]["code"], "value_invalid");
        assert_eq!(invalid_file["file_error"]["layer"], "workspace");
        assert_eq!(invalid_file["file_error"]["path"], json!(workspace));
    }
    assert!(matches!(legacy_rx.try_recv(), Err(TryRecvError::Empty)));
    assert!(matches!(opted_rx.try_recv(), Err(TryRecvError::Empty)));

    rig.set_projects(vec![member("A", Some("W"), &[]), member("C", None, &[])]);
    let dropped = event(&mut opted_rx).await;
    assert_eq!(dropped["event"], "resync");
    assert_eq!(dropped["modules"].as_array().unwrap().len(), 2);
    assert_eq!(dropped["modules"][1]["project_id"], "A");
    assert!(matches!(legacy_rx.try_recv(), Err(TryRecvError::Empty)));
    handler.cleanup_connection(opted.connection_id).unwrap();
    handler.cleanup_connection(legacy.connection_id).unwrap();
    #[cfg(target_os = "linux")]
    project_vectors::assert_watch_samples(
        [
            ("project-watch.request.json", watch),
            ("project-watch.resync.json", initial),
            ("project-watch.change.json", invalid_b),
            ("project-watch.drop.resync.json", dropped),
        ],
        &rig,
    );
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
