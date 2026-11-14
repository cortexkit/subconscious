use super::*;
use serde_json::{json, Value};

fn frame(body: Value) -> Frame {
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        91,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn config_gate_ops_are_explicitly_unprivileged_and_inactive_dispatch_is_named() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let ctx = RouteCtx {
        connection_id: ConnectionId::new(11),
        egress: crate::FrameSink::new(tx),
    };
    for body in [
        json!({"op":"config.status"}),
        json!({"op":"config.plan"}),
        json!({"op":"config.apply","digest":subc_config::raw_digest(b"plan")}),
        json!({"op":"config.stamp"}),
        json!({"op":"config.unstamp"}),
        json!({"op":"config.history"}),
        json!({"op":"config.rollback","generation":1}),
    ] {
        let request: ConfigControlRequest = serde_json::from_value(body.clone()).unwrap();
        assert!(privileged_operation(&request).is_none(), "{}", request.op());
        assert!(SUBC_UNADVERTISED_CONTROL_OPS.contains(&request.op()));
        let response = handler
            .handle_control_frame(&ctx, frame(body))
            .await
            .unwrap();
        assert_eq!(response.len(), 1);
        assert_eq!(response[0].header.ty, FrameType::Error);
        assert_eq!(
            serde_json::from_slice::<Value>(&response[0].body).unwrap()["code"],
            "config_gate_inactive"
        );
    }
    for body in [
        json!({"op":"config.apply"}),
        json!({"op":"config.plan","reviewed":true}),
    ] {
        let response = handler
            .handle_control_frame(&ctx, frame(body))
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&response[0].body).unwrap()["code"],
            "invalid_control_body"
        );
    }
}

/// A re-review owed to a module's newly approved, active program does not hold
/// up other planning when that program cannot answer: the plan proceeds
/// without the module, status still names it, and the next plan asks again.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn config_plan_proceeds_without_an_unavailable_rereview_and_keeps_it_owed() {
    use crate::config_gate::{
        capture::{ConfigCapturer, UnavailableProjectRegistry},
        runtime::{AppliedDisposition, DeliveryObserver, RuntimeOwner},
        ApprovalFacts, ApprovalReply, PlanKind, PresenceDecision,
    };
    use std::io::Write;
    use subc_protocol::config::{
        ConfigAppliedRequest, ConfigApplyResult, ConfigDeclareRequest, ConfigGetRequest,
    };

    let root = cortexkit_test_support::ScratchDir::new("unavailable-rereview");
    std::fs::create_dir_all(root.join("store")).unwrap();
    std::fs::create_dir_all(root.join("cortexkit")).unwrap();
    std::fs::copy("/bin/sleep", root.join("program")).unwrap();
    let path = root.join("subc.jsonc");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({"version": 1, "modules": {"a": {
            "program": root.join("program"), "args": ["600"], "protocol": "none"
        }}}))
        .unwrap(),
    )
    .unwrap();
    let owner = Arc::new(RuntimeOwner::new(
        "unavailable-rereview".into(),
        crate::config_store::ConfigStore::open_digest_only_for_tests(
            crate::config_store::StoreRoot::from_service_definition(root.join("store")).unwrap(),
        ),
        ConfigCapturer {
            daemon_path: path,
            config_home: root.to_path_buf(),
            projects: Arc::new(UnavailableProjectRegistry),
        },
    ));
    let declaration = ConfigDeclareRequest::new(
        json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","unevaluatedProperties":false,"properties":{"limit":{"type":"integer","description":"Limit","default":1}},"x-ck-layers":["user"],"x-ck-apply":"live"}),
        root.join("cortexkit/a.jsonc").to_string_lossy(),
        vec![],
    );
    let facts = ApprovalFacts {
        connection: 7,
        owner_uid: true,
        desktop_app: true,
        attested_operator_app: true,
        os_prompt_available: true,
        phone: None,
        caller: crate::caller_identity::CallerIdentity::default(),
    };
    let approve = |plan: subc_control::config::ConfigPlanView, now: u64| {
        owner
            .desktop_view_delivered(ConnectionId::new(7), &plan)
            .unwrap();
        let challenge = owner.begin_approval(&plan.digest, &facts).unwrap();
        owner
            .finish_approval(
                challenge.attempt(),
                &facts,
                ApprovalReply::Presence(PresenceDecision::InteractiveApproval),
                now,
            )
            .unwrap()
    };
    let acknowledge = |connection: ConnectionId| {
        let reply = owner
            .module_get(connection, "a", ConfigGetRequest::new())
            .unwrap();
        owner
            .module_view_delivered(connection, "a", &reply, tokio::time::Instant::now(), true)
            .unwrap();
        let view = reply.pending.unwrap();
        owner
            .module_applied(
                connection,
                "a",
                ConfigAppliedRequest::new(
                    view.generation,
                    view.attempt.unwrap(),
                    ConfigApplyResult::Ack,
                    vec![],
                ),
            )
            .unwrap()
    };
    let rereview_owed = || {
        owner
            .status()
            .unwrap()
            .warnings
            .iter()
            .any(|warning| warning.starts_with("module a: needs configuration re-review"))
    };

    owner
        .declare(ConnectionId::new(10), "a", declaration.clone(), true)
        .unwrap();
    let stamp = owner
        .finish_plan(
            owner.prepare(PlanKind::Stamp).unwrap(),
            crate::config_gate::runtime::daemon_review,
        )
        .unwrap();
    assert_eq!(approve(stamp, 1), 1);
    assert_eq!(
        acknowledge(ConnectionId::new(10)),
        AppliedDisposition::Active
    );
    std::fs::OpenOptions::new()
        .append(true)
        .open(root.join("program"))
        .unwrap()
        .write_all(b"new approved executable content")
        .unwrap();
    let launch_only = owner
        .finish_plan(
            owner.prepare(PlanKind::Change).unwrap(),
            crate::config_gate::runtime::daemon_review,
        )
        .unwrap();
    assert_eq!(approve(launch_only, 2), 2);
    let spawn = owner
        .spawned(
            "a",
            tokio::time::Instant::now(),
            std::time::Duration::from_secs(120),
            std::time::Duration::from_secs(5),
        )
        .unwrap()
        .unwrap();
    let replacement = ConnectionId::new(30);
    owner
        .declare(replacement, "a", declaration.clone(), true)
        .unwrap();
    owner.registered(replacement, &spawn).unwrap();
    assert_eq!(
        acknowledge(replacement),
        AppliedDisposition::WaitingForReadiness
    );
    let probe = owner.health_probe(replacement, "a").unwrap().unwrap();
    assert_eq!(
        owner.health_result(probe, true).unwrap(),
        Some(AppliedDisposition::Active)
    );
    assert!(rereview_owed());
    assert!(owner
        .prepare(PlanKind::Change)
        .unwrap()
        .rereview_modules
        .contains("a"));

    // No registration answers for "a", so its re-review is unavailable.
    let handler =
        ControlHandler::new(Arc::new(Registry::default())).with_config_runtime(owner.clone());
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let ctx = RouteCtx {
        connection_id: ConnectionId::new(11),
        egress: crate::FrameSink::new(tx),
    };
    let response = handler
        .execute_config_request(
            &ctx,
            &frame(json!({"op":"config.plan"})),
            owner.clone(),
            ConfigControlRequest::Plan {},
        )
        .await
        .expect("an unavailable re-review does not fail the plan");
    let ConfigControlResponse::Plan { view } = response else {
        panic!("config.plan answers with a plan");
    };
    assert!(
        !view
            .plan
            .changeset
            .policy_changes
            .iter()
            .any(|change| change["path"] == "reviewed_program"),
        "the plan binds no re-review it did not perform"
    );
    assert!(rereview_owed(), "status still names the owed re-review");
    let next = owner.prepare(PlanKind::Change).unwrap();
    assert!(next.review_modules.contains("a"));
    assert!(next.rereview_modules.contains("a"));
}
