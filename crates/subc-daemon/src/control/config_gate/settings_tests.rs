use super::*;
use crate::config_gate::runtime::settings::tests::SettingsRig;
use serde_json::{json, Value};

fn request(body: Value) -> Frame {
    Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        91,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}

fn handler(rig: &SettingsRig) -> ControlHandler {
    let handler =
        ControlHandler::new(Arc::new(Registry::default())).with_config_runtime(rig.owner.clone());
    #[cfg(unix)]
    let observed = subc_protocol::peer::ObservedPeer::observed_uid(subc_os::effective_uid());
    #[cfg(windows)]
    let observed = subc_protocol::peer::ObservedPeer::observed_sid(
        subc_os::named_pipe::current_user_sid_string().unwrap(),
    );
    #[cfg(any(unix, windows))]
    handler
        .observed_peers
        .lock()
        .unwrap()
        .insert(ConnectionId::new(77), observed);
    handler
}

fn context() -> (
    RouteCtx,
    tokio::sync::mpsc::Receiver<crate::router::OutboundFrame>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    (
        RouteCtx {
            connection_id: ConnectionId::new(77),
            egress: crate::FrameSink::new(tx),
        },
        rx,
    )
}

async fn event(rx: &mut tokio::sync::mpsc::Receiver<crate::router::OutboundFrame>) -> Value {
    let frame = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap()
        .frame;
    assert_eq!(
        frame.header.ty,
        FrameType::StreamData,
        "unexpected settings watch frame body: {}",
        String::from_utf8_lossy(&frame.body)
    );
    assert_eq!(frame.header.corr, 91);
    serde_json::from_slice(&frame.body).unwrap()
}

#[tokio::test]
async fn config_settings_handler_refusals_and_capability_gate() {
    let rig = SettingsRig::new();
    let handler = handler(&rig);
    let (mut ctx, _rx) = context();
    assert!(handler
        .describe_capabilities()
        .contains(&subc_control::config::CONFIG_SETTINGS_CAPABILITY.to_owned()));
    let described = handler
        .handle_control_frame(&ctx, request(json!({"op":"server.describe"})))
        .await
        .unwrap();
    let described: Value = serde_json::from_slice(&described[0].body).unwrap();
    assert!(described["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|capability| capability == "config.settings/v1"));
    assert!(!subc_ops().iter().any(|op| op == "config.modules"));
    for (body, code) in [
        (
            json!({"op":"config.schema","module_id":"unknown"}),
            "unknown_module",
        ),
        (
            json!({"op":"config.show","module_id":"unknown"}),
            "unknown_module",
        ),
        (
            json!({"op":"config.show","module_id":"sample-reader","project_id":"unknown"}),
            "unknown_project",
        ),
        (
            json!({"op":"config.show","module_id":"sample-reader","through":"project"}),
            "invalid_control_body",
        ),
        (json!({"op":"config.schema"}), "invalid_control_body"),
        (
            json!({"op":"config.modules","extra":true}),
            "invalid_control_body",
        ),
        (
            json!({"op":"config.watch","modules":"sample-reader"}),
            "invalid_control_body",
        ),
    ] {
        let reply = handler
            .handle_control_frame(&ctx, request(body))
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&reply[0].body).unwrap()["code"],
            code
        );
    }
    ctx.connection_id = ConnectionId::new(78);
    for body in [
        json!({"op":"config.modules"}),
        json!({"op":"config.schema","module_id":"sample-reader"}),
        json!({"op":"config.show","module_id":"sample-reader"}),
        json!({"op":"config.watch"}),
    ] {
        let reply = handler
            .handle_control_frame(&ctx, request(body))
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&reply[0].body).unwrap()["code"],
            "owner_required"
        );
    }
}

#[test]
fn config_settings_watch_resync_replaces_added_removed_and_stamp_state() {
    let mut row = ConfigModule::default();
    row.module_id = "sample-reader".into();
    row.status = "adopted".into();
    for (before, stamped, after, next_stamped) in [
        (vec![], false, vec![row.clone()], false),
        (vec![row.clone()], false, vec![], false),
        (vec![row.clone()], false, vec![row.clone()], true),
    ] {
        assert_eq!(
            settings_events(&before, stamped, &after, next_stamped),
            vec![ConfigWatchEvent::Resync {
                modules: after,
                stamped: next_stamped
            }]
        );
    }
    assert!(settings_events(&[row.clone()], true, &[row.clone()], true).is_empty());
    let mut renamed = row.clone();
    renamed.display_name = Some("A new title".into());
    assert!(settings_events(&[row.clone()], true, &[renamed], true).is_empty());
    let mut changed = row.clone();
    changed.pending_plan = Some(subc_config::raw_digest(b"plan"));
    assert_eq!(
        settings_events(&[row], true, &[changed.clone()], true),
        vec![ConfigWatchEvent::change(&changed)]
    );
}

#[tokio::test]
async fn config_settings_watch_filters_reopens_cancels_and_cleans_up() {
    let rig = SettingsRig::new();
    let handler = handler(&rig);
    let (ctx, mut rx) = context();
    let watch = json!({"op":"config.watch","modules":["missing"]});
    assert!(handler
        .handle_control_frame(&ctx, request(watch.clone()))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(event(&mut rx).await["modules"], json!([]));
    let duplicate = handler
        .handle_control_frame(&ctx, request(watch))
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&duplicate[0].body).unwrap()["code"],
        "duplicate_subscription"
    );
    let cancel = Frame::build(FrameType::Cancel, control_flags(), 0, 0, 91, vec![]).unwrap();
    assert!(handler
        .handle_control_frame(&ctx, cancel)
        .await
        .unwrap()
        .is_empty());
    assert!(handler
        .handle_control_frame(&ctx, request(json!({"op":"config.watch"})))
        .await
        .unwrap()
        .is_empty());
    let initial = event(&mut rx).await;
    assert_eq!(initial["event"], "resync");
    assert_eq!(initial["modules"].as_array().unwrap().len(), 1);
    rig.declare("sample-extra", 11, false);
    let added = event(&mut rx).await;
    assert_eq!(added["event"], "resync");
    assert_eq!(added["modules"].as_array().unwrap().len(), 2);
    rig.owner.disconnected(ConnectionId::new(11));
    let removed = event(&mut rx).await;
    assert_eq!(removed["event"], "resync");
    assert_eq!(removed["modules"].as_array().unwrap().len(), 1);
    rig.stamp();
    // Planning the stamp can announce a pending plan before the stamp flips.
    // Those changes are valid until the replacement resync arrives.
    let stamped = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let next = event(&mut rx).await;
            match serde_json::from_value::<ConfigWatchEvent>(next.clone()).unwrap() {
                ConfigWatchEvent::Resync { .. } => break next,
                ConfigWatchEvent::Change {
                    module_id, status, ..
                } => {
                    assert!(removed["modules"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|module| module["module_id"] == module_id));
                    assert!(!status.is_empty());
                }
                other => panic!("unexpected watch event: {other:?}"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(stamped["event"], "resync");
    assert_eq!(stamped["stamped"], true);
    assert_eq!(stamped["modules"].as_array().unwrap().len(), 1);
    assert_eq!(stamped["modules"][0]["module_id"], "sample-reader");
    rig.activate();
    let change = event(&mut rx).await;
    assert_eq!(change["event"], "change");
    assert_eq!(change["active_generation"], 1);
    assert!(handler.cancel_config_settings_subscription(ctx.connection_id, 91));
    assert!(handler
        .handle_control_frame(&ctx, request(json!({"op":"config.watch"})))
        .await
        .unwrap()
        .is_empty());
    let reopened = event(&mut rx).await;
    assert_eq!(reopened["event"], "resync");
    assert_eq!(reopened["modules"][0]["active_generation"], 1);
    handler.cleanup_connection(ctx.connection_id).unwrap();
    assert!(!handler.cancel_config_settings_subscription(ctx.connection_id, 91));
}

#[tokio::test]
async fn config_settings_watch_retries_initial_capture_race() {
    let rig = SettingsRig::new();
    rig.stamp();
    rig.activate();
    let raced = race_settings_captures(&rig, 1);
    let handler = handler(&rig);
    let (ctx, mut rx) = context();
    assert!(handler
        .handle_control_frame(&ctx, request(json!({"op":"config.watch"})))
        .await
        .unwrap()
        .is_empty());
    let initial = event(&mut rx).await;
    assert_eq!(initial["event"], "resync");
    assert_eq!(initial["modules"][0]["status"], "formatting_only");
    assert_eq!(raced.load(std::sync::atomic::Ordering::SeqCst), 1);
    handler.cleanup_connection(ctx.connection_id).unwrap();
}

#[tokio::test]
async fn config_settings_watch_retries_ongoing_capture_race() {
    let rig = SettingsRig::new();
    rig.stamp();
    rig.activate();
    let handler = handler(&rig);
    let (ctx, mut rx) = context();
    handler
        .handle_control_frame(&ctx, request(json!({"op":"config.watch"})))
        .await
        .unwrap();
    assert_eq!(event(&mut rx).await["modules"][0]["status"], "adopted");
    let raced = race_settings_captures(&rig, 1);
    let changed = event(&mut rx).await;
    assert_eq!(changed["event"], "change");
    assert_eq!(changed["status"], "formatting_only");
    assert_eq!(raced.load(std::sync::atomic::Ordering::SeqCst), 1);
    rig.write(br#"{"limit":99,"credential":{"vault_ref":"sample-reference"}}"#);
    assert_eq!(event(&mut rx).await["status"], "pending_proposal");
    handler.cleanup_connection(ctx.connection_id).unwrap();
}

fn race_settings_captures(
    rig: &SettingsRig,
    mut remaining: usize,
) -> Arc<std::sync::atomic::AtomicUsize> {
    let path = rig.root.join("cortexkit/sample-reader.jsonc");
    let raced = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = raced.clone();
    *rig.owner.settings_projection_hook.lock().unwrap() = Some(Box::new(move || {
        if remaining == 0 {
            return Ok(());
        }
        remaining -= 1;
        // Return an actual capture refusal caused by a rewrite between the read
        // and its consistency check, at the settings projection boundary.
        let error = crate::config_gate::capture::CapturedFile::open_with_read_hook(&path, || {
            let mut bytes = std::fs::read(&path).unwrap();
            bytes.push(b' ');
            std::fs::write(&path, bytes).unwrap();
        })
        .err()
        .expect("the injected same-file rewrite must invalidate capture");
        assert_eq!(error.code, "plan_stale");
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(error)
    }));
    raced
}

#[tokio::test]
async fn config_settings_watch_refuses_persistent_initial_capture_race() {
    let rig = SettingsRig::new();
    rig.stamp();
    rig.activate();
    let raced = race_settings_captures(&rig, usize::MAX);
    let handler = handler(&rig);
    let (ctx, mut rx) = context();
    let refused = tokio::time::timeout(
        Duration::from_secs(3),
        handler.handle_control_frame(&ctx, request(json!({"op":"config.watch"}))),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].header.ty, FrameType::Error);
    let body: Value = serde_json::from_slice(&refused[0].body).unwrap();
    assert_eq!(body["code"], "plan_stale");
    assert_eq!(
        body["message"],
        "reviewed inputs or generation changed; plan again"
    );
    assert_eq!(raced.load(std::sync::atomic::Ordering::SeqCst), 4);
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    handler.cleanup_connection(ctx.connection_id).unwrap();
}

#[tokio::test]
async fn config_settings_watch_does_not_retry_other_refusals() {
    for error in [
        GateError::new(
            "plan_stale",
            "reviewed inputs or generation changed; plan again",
        ),
        GateError::new("config_unavailable", "configuration is unavailable"),
    ] {
        let rig = SettingsRig::new();
        rig.stamp();
        rig.activate();
        let handler = handler(&rig);
        let (ctx, mut rx) = context();
        let fault = error.clone();
        *rig.owner.settings_projection_hook.lock().unwrap() =
            Some(Box::new(move || Err(fault.clone())));
        let refused = handler
            .handle_control_frame(&ctx, request(json!({"op":"config.watch"})))
            .await
            .unwrap();
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].header.ty, FrameType::Error);
        assert_eq!(
            serde_json::from_slice::<Value>(&refused[0].body).unwrap()["code"],
            error.code
        );
        *rig.owner.settings_projection_hook.lock().unwrap() = None;
        handler
            .handle_control_frame(&ctx, request(json!({"op":"config.watch"})))
            .await
            .unwrap();
        assert_eq!(event(&mut rx).await["event"], "resync");
        let fault = error.clone();
        *rig.owner.settings_projection_hook.lock().unwrap() =
            Some(Box::new(move || Err(fault.clone())));
        let terminal = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap()
            .frame;
        assert_eq!(terminal.header.ty, FrameType::Error);
        assert_eq!(
            serde_json::from_slice::<Value>(&terminal.body).unwrap()["code"],
            error.code
        );
        handler.cleanup_connection(ctx.connection_id).unwrap();
    }
}

#[tokio::test]
async fn config_settings_watch_survives_truncated_save() {
    probe_save_window(false).await;
}

#[test]
fn config_settings_watch_file_error_grace_uses_matching_spaced_observations() {
    let mut good = ConfigModule::default();
    good.module_id = "sample".into();
    good.status = "adopted".into();
    let mut bad = good.clone();
    bad.file_error = Some(ConfigFileError::new("value_invalid", "invalid JSON"));
    let start = tokio::time::Instant::now();
    let mut grace = SettingsFileGrace::default();
    for age in [0, 1] {
        let mut rows = vec![bad.clone()];
        grace.settle(&[good.clone()], &mut rows, start + Duration::from_secs(age));
        assert_eq!(rows, vec![good.clone()]);
    }
    let mut rows = vec![bad.clone()];
    grace.settle(&[good.clone()], &mut rows, start + Duration::from_secs(2));
    assert_eq!(rows, vec![bad.clone()]);
    let mut changed_error = bad.clone();
    changed_error.file_error = Some(ConfigFileError::new(
        "input_unavailable",
        "unreadable JSONC",
    ));
    let mut rows = vec![changed_error.clone()];
    grace.settle(&[bad.clone()], &mut rows, start + Duration::from_secs(3));
    assert_eq!(rows, vec![bad.clone()]);
    let mut rows = vec![changed_error.clone()];
    grace.settle(&[bad], &mut rows, start + Duration::from_secs(5));
    assert_eq!(rows, vec![changed_error]);
    let mut rows = vec![good.clone()];
    grace.settle(&[], &mut rows, start + Duration::from_secs(6));
    assert_eq!(rows, vec![good]);
    assert!(grace.pending.is_empty());
}

#[tokio::test]
async fn config_settings_watch_unstamped_file_error_does_not_invent_values() {
    let rig = SettingsRig::new();
    rig.write(b"{");
    let handler = handler(&rig);
    let (ctx, mut rx) = context();
    assert!(handler
        .handle_control_frame(&ctx, request(json!({"op":"config.watch"})))
        .await
        .unwrap()
        .is_empty());
    let initial = event(&mut rx).await;
    assert_eq!(initial["stamped"], false);
    let row = &initial["modules"][0];
    assert_eq!(row["file_error"]["code"], "value_invalid");
    assert!(row.get("effective").is_none());
    assert!(row.get("layers").is_none());
    let show = handler
        .handle_control_frame(
            &ctx,
            request(json!({"op":"config.show","module_id":"sample-reader"})),
        )
        .await
        .unwrap();
    assert_eq!(show[0].header.ty, FrameType::Error);
    let refused: Value = serde_json::from_slice(&show[0].body).unwrap();
    assert_eq!(refused, row["file_error"]);
    rig.write(br#"{"limit":9,"credential":{"vault_ref":"sample-reference"}}"#);
    let recovered = event(&mut rx).await;
    assert!(recovered.get("file_error").is_none());
    let show = handler
        .handle_control_frame(
            &ctx,
            request(json!({"op":"config.show","module_id":"sample-reader"})),
        )
        .await
        .unwrap();
    let show: Value = serde_json::from_slice(&show[0].body).unwrap();
    assert_eq!(show["effective"]["limit"], 9);
    assert_eq!(show["base_generation"], 0);
    handler.cleanup_connection(ctx.connection_id).unwrap();
}

#[tokio::test]
async fn config_settings_watch_survives_partial_save() {
    probe_save_window(true).await;
}

async fn probe_save_window(partial: bool) {
    use std::io::Write;
    let rig = SettingsRig::new();
    rig.stamp();
    rig.activate();
    rig.edit_limit(8);
    rig.activate_generation(2);
    let handler = handler(&rig);
    let (ctx, mut rx) = context();
    handler
        .handle_control_frame(&ctx, request(json!({"op":"config.watch"})))
        .await
        .unwrap();
    assert_eq!(event(&mut rx).await["modules"][0]["status"], "adopted");
    let target = br#"{"limit":99,"credential":{"vault_ref":"sample-reference"}}"#.to_vec();
    let path = rig.root.join("cortexkit/sample-reader.jsonc");
    let writer = Arc::new(std::sync::Mutex::new(None));
    let held = writer.clone();
    let prefix_len = if partial { target.len() / 2 } else { 0 };
    let bytes = target.clone();
    let mut injected = false;
    *rig.owner.settings_projection_hook.lock().unwrap() = Some(Box::new(move || {
        if !injected {
            injected = true;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&path)
                .unwrap();
            file.write_all(&bytes[..prefix_len]).unwrap();
            *held.lock().unwrap() = Some(file);
        }
        Ok(())
    }));
    let held = writer.clone();
    let suffix = target[prefix_len..].to_vec();
    let mut finished = false;
    *rig.owner.settings_observation_hook.lock().unwrap() = Some(Box::new(move || {
        if !finished {
            if let Some(file) = held.lock().unwrap().as_mut() {
                // Finish the save only after the observation has copied and
                // rejected the incomplete bytes, not before its capture read.
                file.write_all(&suffix).unwrap();
                finished = true;
            }
        }
    }));
    let changed = event(&mut rx).await;
    assert_eq!(changed["event"], "change");
    assert_eq!(changed["status"], "write_back_behind");
    assert!(changed.get("file_error").is_none());
    assert_eq!(
        std::fs::read(rig.root.join("cortexkit/sample-reader.jsonc")).unwrap(),
        target
    );
    handler.cleanup_connection(ctx.connection_id).unwrap();
}

#[tokio::test]
async fn config_settings_watch_reports_persistent_file_error_and_recovers() {
    let rig = SettingsRig::new();
    rig.stamp();
    rig.activate();
    let approved = rig.edit_limit(8);
    rig.activate_generation(2);
    let handler = handler(&rig);
    let (ctx, mut rx) = context();
    handler
        .handle_control_frame(&ctx, request(json!({"op":"config.watch"})))
        .await
        .unwrap();
    assert_eq!(event(&mut rx).await["modules"][0]["status"], "adopted");
    let since = tokio::time::Instant::now();
    rig.write(b"{");
    let failed = event(&mut rx).await;
    assert_eq!(failed["event"], "change");
    assert_eq!(failed["file_error"]["code"], "value_invalid");
    assert_eq!(
        failed["file_error"]["message"],
        "configuration must be JSON with unique keys and lossless finite numbers"
    );
    assert!(since.elapsed() >= Duration::from_secs(2));
    let show = handler
        .handle_control_frame(
            &ctx,
            request(json!({"op":"config.show","module_id":"sample-reader"})),
        )
        .await
        .unwrap();
    let show: Value = serde_json::from_slice(&show[0].body).unwrap();
    assert_eq!(show["effective"]["limit"], 8);
    assert_eq!(show["base_generation"], 2);
    assert_eq!(show["file_error"], failed["file_error"]);
    rig.write(&approved);
    let recovered = event(&mut rx).await;
    assert_eq!(recovered["event"], "change");
    assert!(recovered.get("file_error").is_none());
    assert_eq!(recovered["status"], "adopted");
    assert!(handler.cancel_config_settings_subscription(ctx.connection_id, 91));
    rig.write(b"{");
    assert!(handler
        .handle_control_frame(&ctx, request(json!({"op":"config.watch"})))
        .await
        .unwrap()
        .is_empty());
    let opened = event(&mut rx).await;
    assert_eq!(opened["event"], "resync");
    assert_eq!(opened["modules"][0]["file_error"]["code"], "value_invalid");
    handler.cleanup_connection(ctx.connection_id).unwrap();
}

#[tokio::test]
async fn config_settings_watch_skips_unstable_status_without_aging_file_error() {
    let rig = SettingsRig::new();
    rig.stamp();
    rig.activate();
    let approved = rig.edit_limit(8);
    rig.activate_generation(2);
    let handler = handler(&rig);
    let (ctx, mut rx) = context();
    handler
        .handle_control_frame(&ctx, request(json!({"op":"config.watch"})))
        .await
        .unwrap();
    assert_eq!(event(&mut rx).await["modules"][0]["status"], "adopted");
    let path = rig.root.join("cortexkit/sample-reader.jsonc");
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = attempts.clone();
    *rig.owner.settings_status_hook.lock().unwrap() = Some(Box::new(move || {
        let attempt = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::fs::write(
            &path,
            if attempt.is_multiple_of(2) {
                b"{"
            } else {
                &approved[..]
            },
        )
        .unwrap();
        Ok(())
    }));
    let (observed, mut observations) = tokio::sync::mpsc::unbounded_channel();
    *rig.owner.settings_observation_hook.lock().unwrap() = Some(Box::new(move || {
        observed.send(()).unwrap();
    }));
    // Let inconsistent captures span a full grace interval. They must neither
    // publish a warning nor help a later, genuinely bad file pass the grace.
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(5), observations.recv())
            .await
            .unwrap()
            .unwrap();
    }
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 6);
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    *rig.owner.settings_status_hook.lock().unwrap() = None;
    rig.write(b"{");
    let since = tokio::time::Instant::now();
    let failed = event(&mut rx).await;
    assert_eq!(failed["file_error"]["code"], "value_invalid");
    assert!(since.elapsed() >= Duration::from_secs(2));
    handler.cleanup_connection(ctx.connection_id).unwrap();
}

#[tokio::test]
async fn config_settings_watch_reports_write_back_status_and_reason_changes() {
    let rig = SettingsRig::new();
    rig.stamp();
    rig.activate();
    let approved = rig.edit_limit(8);
    rig.activate_generation(2);
    let handler = handler(&rig);
    let (ctx, mut rx) = context();
    handler
        .handle_control_frame(&ctx, request(json!({"op":"config.watch"})))
        .await
        .unwrap();
    assert_eq!(event(&mut rx).await["modules"][0]["status"], "adopted");
    rig.write(br#"{"limit":99,"credential":{"vault_ref":"sample-reference"}}"#);
    let behind = event(&mut rx).await;
    assert_eq!(behind["event"], "change");
    assert_eq!(behind["status"], "write_back_behind");
    assert_eq!(
        behind["status_reason"],
        "the file changed after this was approved"
    );
    rig.write(&approved);
    let adopted = event(&mut rx).await;
    assert_eq!(adopted["event"], "change");
    assert_eq!(adopted["status"], "adopted");
    assert!(adopted.get("status_reason").is_none());
    handler.cleanup_connection(ctx.connection_id).unwrap();
}

#[tokio::test]
async fn config_settings_vectors_match_real_handler_bytes() {
    use sha2::{Digest, Sha256};
    let rig = SettingsRig::new();
    rig.stamp();
    let handler = handler(&rig);
    let (ctx, mut rx) = context();
    let mut samples = Vec::new();
    for (name, body) in [
        ("modules", json!({"op":"config.modules"})),
        (
            "schema",
            json!({"op":"config.schema","module_id":"sample-reader"}),
        ),
        (
            "show",
            json!({"op":"config.show","module_id":"sample-reader"}),
        ),
        (
            "show-default",
            json!({"op":"config.show","module_id":"sample-reader","through":"default"}),
        ),
    ] {
        let reply = handler
            .handle_control_frame(&ctx, request(body.clone()))
            .await
            .unwrap();
        assert_eq!(reply[0].header.ty, FrameType::Response);
        samples.push((format!("{name}.request.json"), body));
        samples.push((
            format!("{name}.reply.json"),
            serde_json::from_slice::<Value>(&reply[0].body).unwrap(),
        ));
    }
    let watch = json!({"op":"config.watch","modules":["sample-reader"]});
    assert!(handler
        .handle_control_frame(&ctx, request(watch.clone()))
        .await
        .unwrap()
        .is_empty());
    samples.push(("watch.request.json".into(), watch));
    samples.push(("watch.resync.json".into(), event(&mut rx).await));
    rig.activate();
    let change = event(&mut rx).await;
    assert_eq!(change["event"], "change");
    samples.push(("watch.change.json".into(), change));
    rig.write(b"{");
    let failed = event(&mut rx).await;
    assert_eq!(failed["file_error"]["code"], "value_invalid");
    samples.push(("watch.file-error.json".into(), failed));
    let shown = handler
        .handle_control_frame(
            &ctx,
            request(json!({"op":"config.show","module_id":"sample-reader"})),
        )
        .await
        .unwrap();
    assert_eq!(shown[0].header.ty, FrameType::Response);
    let shown: Value = serde_json::from_slice(&shown[0].body).unwrap();
    assert_eq!(shown["effective"]["limit"], 4);
    samples.push(("show.file-error.reply.json".into(), shown));
    handler.cleanup_connection(ctx.connection_id).unwrap();

    let setup_rig = SettingsRig::new();
    let setup_schema = json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "type":"object","unevaluatedProperties":false,
        "x-ck-layers":["user"],"x-ck-apply":"live",
        "properties":{
            "journal":{
                "type":"object","title":"Journal reads",
                "description":"Read entries from a journal.","unevaluatedProperties":false,
                "x-ck-setup":{"feature":"journal_reads","order":10,
                    "recommended":{"/journal/read":true}},
                "properties":{"read":{"type":"boolean","description":"Read entries","default":false}}
            },
            "archive":{
                "type":"object","title":"Archive writes",
                "description":"Write entries to an archive.","unevaluatedProperties":false,
                "x-ck-setup":{"feature":"archive_writes","order":20,"recommended":{}},
                "properties":{"write":{"type":"boolean","description":"Write entries","default":false}}
            }
        }
    });
    let declare = |schema| {
        setup_rig.owner.declare(
            ConnectionId::new(12),
            "sample-setup",
            subc_protocol::config::ConfigDeclareRequest::new(
                schema,
                setup_rig
                    .root
                    .join("cortexkit/sample-setup.jsonc")
                    .to_string_lossy(),
                vec![],
            ),
            true,
        )
    };
    let mut invalid = setup_schema.clone();
    invalid["properties"]["journal"]["x-ck-setup"]["feature"] = json!("archive_writes");
    let refusal = declare(invalid).unwrap_err();
    assert_eq!(refusal.code, "schema_invalid");
    assert!(refusal.detail.contains("/journal"));
    declare(setup_schema.clone()).unwrap();
    let setup_handler = self::handler(&setup_rig);
    let body = json!({"op":"config.schema","module_id":"sample-setup"});
    let reply = setup_handler
        .handle_control_frame(&ctx, request(body.clone()))
        .await
        .unwrap();
    assert_eq!(reply[0].header.ty, FrameType::Response);
    let reply: Value = serde_json::from_slice(&reply[0].body).unwrap();
    for section in ["journal", "archive"] {
        assert_eq!(
            reply["schema"]["properties"][section]["x-ck-setup"],
            setup_schema["properties"][section]["x-ck-setup"]
        );
    }
    samples.push(("schema-setup.request.json".into(), body));
    samples.push(("schema-setup.reply.json".into(), reply));
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vectors/config-settings-v1");
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let mut sums = Vec::new();
    for (name, value) in samples {
        let canonical = subc_config::canonical_json(&value).unwrap();
        let value: Value = serde_json::from_slice(&canonical).unwrap();
        let mut bytes = serde_json::to_vec_pretty(&value).unwrap();
        bytes.push(b'\n');
        if update {
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join(&name), &bytes).unwrap();
        }
        assert_eq!(
            std::fs::read(directory.join(&name)).unwrap(),
            bytes,
            "{name}; regenerate with UPDATE_GOLDEN=1"
        );
        sums.push(format!("{:x}  {name}", Sha256::digest(&bytes)));
    }
    if update {
        let names: Vec<_> = sums
            .iter()
            .map(|row| row.split_once("  ").unwrap().1)
            .collect();
        let mut combined: Vec<String> = std::fs::read_to_string(directory.join("SHA256SUMS"))
            .unwrap_or_default()
            .lines()
            .filter(|row| {
                row.split_once("  ")
                    .is_some_and(|(_, name)| !names.contains(&name))
            })
            .map(str::to_owned)
            .collect();
        combined.extend(sums.clone());
        combined.sort_by(|a, b| {
            a.split_once("  ")
                .unwrap()
                .1
                .cmp(b.split_once("  ").unwrap().1)
        });
        std::fs::write(directory.join("SHA256SUMS"), combined.join("\n") + "\n").unwrap();
    }
    let saved = std::fs::read_to_string(directory.join("SHA256SUMS")).unwrap();
    for row in sums {
        assert!(saved.lines().any(|saved| saved == row), "{row}");
    }
}
