use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use subc_protocol::scope::{ScopeAttributes, ScopeEndOutcome, MAX_LIVE_SCOPES_PER_OWNER};

fn set_clock(rig: &mut Rig) -> Arc<AtomicU64> {
    let clock = Arc::new(AtomicU64::new(1_000));
    let source = Arc::clone(&clock);
    rig.handler = rig
        .handler
        .clone()
        .with_wall_clock(move || source.load(Ordering::SeqCst));
    clock
}

async fn apply_call(
    rig: &Rig,
    generation: u64,
    upsert: Vec<ScopeRecord>,
    end: Vec<ScopeEnd>,
) -> Frame {
    tokio::time::timeout(
        Duration::from_secs(2),
        call(
            &rig.handler,
            &rig.owner,
            &ModuleControlRequestFromModule::ScopeApply {
                generation,
                upsert,
                end,
            },
        ),
    )
    .await
    .expect("scope.apply must return without holding a lock across route closure")
}

fn run_scope() -> ScopeRecord {
    ScopeRecord::new("s", 1, ScopeKind::Ephemeral)
        .with_carriers(vec![carrier(AFT, None)])
        .with_attributes(
            ScopeAttributes::new()
                .with_agent_id(Some("agent-1".into()))
                .with_run_id(Some("run:7".into())),
        )
}

#[tokio::test]
async fn handler_sweep_ends_routes_and_child_links_without_owner_calls() {
    for connected in [false, true] {
        let mut rig = rig().await;
        let clock = set_clock(&mut rig);
        let parent = session(1)
            .with_expires_at_ms(Some(2_000))
            .with_child_owners(vec![Principal::Reserved {
                module_id: MAGIC.into(),
            }]);
        rig.sync(vec![parent, head("forever", 1)]).await;
        let (child_owner, mut child_owner_rx) = wide_ctx(50);
        hello_via_sink(
            &rig.handler,
            &child_owner,
            &mut child_owner_rx,
            hello_frame_with_nonce(MAGIC, PROTOCOL_VERSION, 50, Some(&nonce(MAGIC))),
        )
        .await;
        let child = head("child", 1).with_parent(Some(ScopeParent::new(
            Principal::Reserved {
                module_id: OWNER.into(),
            },
            "s",
            1,
        )));
        sync(&rig.handler, &child_owner, 1, vec![child])
            .await
            .unwrap();
        let mut parent_route = rig
            .bound(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
            .await;
        let child_selector = ScopeSelector::new(Principal::Reserved {
            module_id: MAGIC.into(),
        }, "child", Some(1));
        let mut child_route = rig.bound(Some(MAGIC), OTHER, Some(child_selector)).await;
        if !connected {
            rig.handler
                .cleanup_connection(rig.owner.connection_id)
                .unwrap();
        }
        clock.store(2_000, Ordering::SeqCst);
        rig.handler.sweep_expired_scopes().unwrap();
        assert!(!rig.live(&parent_route));
        assert_eq!(parent_route.closed_reason(), RouteCloseReason::ScopeEnded);
        assert!(!rig.live(&child_route));
        assert_eq!(
            child_route.closed_reason(),
            RouteCloseReason::ScopeParentEnded
        );
        let reply = describe(&rig.handler, &child_owner, OWNER, "s").await;
        assert!(matches!(
            reply,
            ModuleControlResponseToModule::ScopeDescribe {
                status: ScopeStatus::Ended,
                scope_epoch: Some(1),
                ..
            }
        ));
        let child = describe(&rig.handler, &child_owner, MAGIC, "child").await;
        assert!(matches!(
            child,
            ModuleControlResponseToModule::ScopeDescribe {
                status: ScopeStatus::Live,
                scope: Some(ScopeStamp {
                    parent_state: Some(ParentState::Ended),
                    ..
                }),
                ..
            }
        ));
        if connected {
            assert_eq!(
                sync(&rig.handler, &rig.owner, 1, vec![]).await.unwrap_err(),
                error_codes::SCOPE_SYNC_STALE
            );
            rig.sync(vec![head("forever", 1)]).await;
        } else {
            let (new_owner, mut new_rx) = wide_ctx(200);
            hello_via_sink(
                &rig.handler,
                &new_owner,
                &mut new_rx,
                hello_frame_with_nonce(OWNER, PROTOCOL_VERSION, 200, Some(&nonce(OWNER))),
            )
            .await;
            let reply = sync(&rig.handler, &new_owner, 0, vec![session(1)])
                .await
                .unwrap();
            let ModuleControlResponseToModule::ScopeSync { results, ended, .. } = reply else {
                panic!("sync reply");
            };
            assert_eq!(results[0].code.as_deref(), Some(error_codes::SCOPE_EXPIRED));
            assert!(ended.iter().all(|entry| entry.scope_ref != "s"));
        }
    }
}

#[tokio::test]
async fn handler_accepted_sync_and_apply_sweep_and_return_once_without_deadlock() {
    for delta in [false, true] {
        let mut rig = rig().await;
        let clock = set_clock(&mut rig);
        let scope = session(1).with_expires_at_ms(Some(2_000));
        rig.sync(vec![scope.clone()]).await;
        let mut route = rig
            .bound(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
            .await;
        let (other_owner, mut other_owner_rx) = wide_ctx(50);
        hello_via_sink(
            &rig.handler,
            &other_owner,
            &mut other_owner_rx,
            hello_frame_with_nonce(BROCA, PROTOCOL_VERSION, 50, Some(&nonce(BROCA))),
        )
        .await;
        sync(
            &rig.handler,
            &other_owner,
            1,
            vec![head("other-s", 1).with_expires_at_ms(Some(2_000))],
        )
        .await
        .unwrap();
        let other_selector = ScopeSelector::new(Principal::Reserved {
            module_id: BROCA.into(),
        }, "other-s", Some(1));
        let mut other_route = rig.bound(Some(BROCA), OTHER, Some(other_selector)).await;
        clock.store(2_000, Ordering::SeqCst);
        let reply = if delta {
            let frame = apply_call(&rig, 2, vec![scope], vec![]).await;
            serde_json::from_slice(&frame.body).unwrap()
        } else {
            tokio::time::timeout(
                Duration::from_secs(2),
                sync(&rig.handler, &rig.owner, 2, vec![scope]),
            )
            .await
            .expect("scope.sync must return without deadlocking route closure")
            .unwrap()
        };
        let (results, ended) = match reply {
            ModuleControlResponseToModule::ScopeApply { results, ended, .. }
            | ModuleControlResponseToModule::ScopeSync { results, ended, .. } => (results, ended),
            other => panic!("unexpected reply {other:?}"),
        };
        assert_eq!(results[0].code.as_deref(), Some(error_codes::SCOPE_EXPIRED));
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].scope_ref, "s");
        assert_eq!(route.closed_reason(), RouteCloseReason::ScopeEnded);
        assert!(rig.live(&other_route));
        assert!(other_route.untouched());
        let other = describe(&rig.handler, &rig.owner, BROCA, "other-s").await;
        assert!(matches!(
            other,
            ModuleControlResponseToModule::ScopeDescribe {
                status: ScopeStatus::Live,
                ..
            }
        ));
        let reply = describe(&rig.handler, &rig.owner, OWNER, "s").await;
        assert!(matches!(
            reply,
            ModuleControlResponseToModule::ScopeDescribe {
                status: ScopeStatus::Ended,
                ..
            }
        ));
    }
}

#[tokio::test]
async fn handler_whole_call_refusals_leave_expired_routes_live_and_tags_unpublished() {
    let mut rig = rig().await;
    let clock = set_clock(&mut rig);
    let mut records: Vec<_> = (1..MAX_LIVE_SCOPES_PER_OWNER)
        .map(|i| head(&i.to_string(), 1))
        .collect();
    records.push(session(1).with_expires_at_ms(Some(2_000)));
    rig.sync(records).await;
    let mut route = rig
        .bound(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
        .await;
    clock.store(2_000, Ordering::SeqCst);
    let stale = apply_call(&rig, 1, vec![], vec![]).await;
    assert_eq!(parse_error(&stale)["code"], error_codes::SCOPE_SYNC_STALE);
    let full = apply_call(&rig, 2, vec![head("new1", 1), head("new2", 1)], vec![]).await;
    assert_eq!(
        parse_error(&full)["code"],
        error_codes::SCOPE_LIVE_LIMIT_EXCEEDED
    );
    assert!(rig.live(&route));
    assert!(route.untouched());
    let reply = describe(&rig.handler, &rig.owner, OWNER, "s").await;
    assert!(matches!(
        reply,
        ModuleControlResponseToModule::ScopeDescribe {
            status: ScopeStatus::Live,
            ..
        }
    ));
    clock.store(1_999, Ordering::SeqCst);
    let pending = rig
        .relayed(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
        .await;
    let accepted = apply_call(&rig, 2, vec![], vec![ScopeEnd::new("missing", 1)]).await;
    assert_eq!(accepted.header.ty, FrameType::Response);
    rig.ack(&pending).await;
    assert!(
        pending.task.await.unwrap().is_empty(),
        "refused calls must not publish a new scope tag"
    );
}

#[tokio::test]
async fn handler_admission_checks_deadline_without_sweeping_and_backward_wall_step_reopens() {
    let mut rig = rig().await;
    let clock = set_clock(&mut rig);
    rig.sync(vec![session(1).with_expires_at_ms(Some(2_000))])
        .await;
    clock.store(2_000, Ordering::SeqCst);
    let refusal = rig
        .refusal_body(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
        .await;
    assert_eq!(refusal["code"], error_codes::SCOPE_NOT_LIVE);
    let message = refusal["message"].as_str().unwrap();
    assert!(message.contains("'s'") && message.contains("2000") && message.contains("passed"));
    let reply = describe(&rig.handler, &rig.owner, OWNER, "s").await;
    assert!(matches!(
        reply,
        ModuleControlResponseToModule::ScopeDescribe {
            status: ScopeStatus::Live,
            ..
        }
    ));
    clock.store(1_999, Ordering::SeqCst);
    let route = rig
        .bound(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
        .await;
    assert!(rig.live(&route));
}

#[tokio::test]
async fn handler_apply_requires_registration_and_full_sync_and_advertises_the_op() {
    let mut rig = rig().await;
    let (client, _client_rx) = wide_ctx(60);
    let request = ModuleControlRequestFromModule::ScopeApply {
        generation: 1,
        upsert: vec![],
        end: vec![],
    };
    assert_eq!(
        parse_error(&call(&rig.handler, &client, &request).await)["code"],
        "not_registered"
    );
    let refused = apply_call(&rig, 999, vec![head("s", 1)], vec![]).await;
    assert_eq!(
        parse_error(&refused)["code"],
        error_codes::SCOPE_SYNC_REQUIRED
    );
    rig.sync(vec![head("s", 1)]).await;
    let reply = apply_call(
        &rig,
        2,
        vec![head("new", 1)],
        vec![ScopeEnd::new("s", 1), ScopeEnd::new("missing", 1)],
    )
    .await;
    let ModuleControlResponseToModule::ScopeApply {
        generation,
        results,
        end_results,
        ended,
    } = serde_json::from_slice(&reply.body).unwrap()
    else {
        panic!("scope.apply reply");
    };
    assert_eq!(generation, 2);
    assert_eq!(results[0].outcome, ScopeRecordOutcome::Created);
    assert_eq!(
        end_results
            .iter()
            .map(|entry| entry.outcome)
            .collect::<Vec<_>>(),
        vec![ScopeEndOutcome::Ended, ScopeEndOutcome::NotLive]
    );
    assert_eq!(ended.len(), 1);
    let (ctx, mut rx) = wide_ctx(70);
    let ack = hello_via_sink(
        &rig.handler,
        &ctx,
        &mut rx,
        hello_frame("cap-check", PROTOCOL_VERSION, 70),
    )
    .await;
    let body: Value = serde_json::from_slice(&ack.body).unwrap();
    assert!(body["subc_ops"]
        .as_array()
        .unwrap()
        .iter()
        .any(|op| op == SCOPE_APPLY_OP));
}

#[tokio::test]
async fn handler_apply_end_drains_parent_routes_and_leaves_child_live() {
    let mut rig = rig().await;
    let parent = session(1);
    let child = head("child", 1).with_parent(Some(ScopeParent::new(
        Principal::Reserved {
            module_id: OWNER.into(),
        },
        "s",
        1,
    )));
    rig.sync(vec![parent, child]).await;
    let mut route = rig
        .bound(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
        .await;
    let reply = apply_call(&rig, 2, vec![], vec![ScopeEnd::new("s", 1)]).await;
    assert_eq!(reply.header.ty, FrameType::Response);
    assert!(!rig.live(&route));
    assert_eq!(route.closed_reason(), RouteCloseReason::ScopeEnded);
    let child = describe(&rig.handler, &rig.owner, OWNER, "child").await;
    assert!(matches!(
        child,
        ModuleControlResponseToModule::ScopeDescribe {
            status: ScopeStatus::Live,
            scope: Some(ScopeStamp {
                parent_state: Some(ParentState::Ended),
                ..
            }),
            ..
        }
    ));
}

#[tokio::test]
async fn run_scope_refuses_target_without_capability_and_relays_nothing() {
    let mut rig = rig_with_scope_capabilities(false, false).await;
    rig.sync(vec![run_scope()]).await;
    let body = rig
        .refusal_body(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
        .await;
    assert_eq!(body["code"], error_codes::TARGET_AGENT_RUN_UNSUPPORTED);
    assert!(!error_codes::is_retryable_route_open(
        error_codes::TARGET_AGENT_RUN_UNSUPPORTED
    ));
    let message = body["message"].as_str().unwrap();
    assert!(message.contains(PLEXUS) && message.contains("agent-run-scopes/v1"));
}

#[tokio::test]
async fn run_scope_admits_capable_target_stamps_run_id_and_drains_only_on_change() {
    let mut rig = rig_with_scope_capabilities(false, true).await;
    let record = run_scope();
    rig.sync(vec![record.clone()]).await;
    let mut route = rig
        .bound(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
        .await;
    assert_eq!(
        route.stamp().unwrap().attributes.run_id.as_deref(),
        Some("run:7")
    );
    let unchanged = apply_call(&rig, 2, vec![record.clone()], vec![]).await;
    let ModuleControlResponseToModule::ScopeApply { results, .. } =
        serde_json::from_slice(&unchanged.body).unwrap()
    else {
        panic!("scope.apply reply");
    };
    assert_eq!(results[0].outcome, ScopeRecordOutcome::Unchanged);
    assert!(route.untouched());
    assert!(rig.live(&route));
    let changed = record.with_attributes(
        ScopeAttributes::new()
            .with_agent_id(Some("agent-1".into()))
            .with_run_id(Some("run:8".into())),
    );
    assert_eq!(
        apply_call(&rig, 3, vec![changed], vec![]).await.header.ty,
        FrameType::Response
    );
    assert_eq!(
        route.closed_reason(),
        RouteCloseReason::ScopeDelegationChanged
    );
}

#[tokio::test]
async fn run_scope_admission_refuses_before_waiting_for_response_permit() {
    let mut rig = rig_with_scope_capabilities(false, false).await;
    rig.sync(vec![run_scope()]).await;
    let (client, _client_rx, frame) =
        rig.open_frame(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))));
    // A full response queue distinguishes admission from the later relay gate:
    // admission must refuse without waiting to reserve the response permit.
    for _ in 0..64 {
        client.egress.try_send(route_bind_ack(1)).unwrap();
    }
    let replies = tokio::time::timeout(
        Duration::from_millis(100),
        rig.handler.handle_control_frame(&client, frame),
    )
    .await
    .expect("run capability refusal must happen at admission, before response reservation")
    .unwrap();
    assert_eq!(
        parse_error(&replies[0])["code"],
        error_codes::TARGET_AGENT_RUN_UNSUPPORTED
    );
    assert!(rig.modules.get_mut(PLEXUS).unwrap().1.try_recv().is_err());
    assert_eq!(rig.forwarding.reserved_route_count().unwrap(), (0, 0));
}

#[tokio::test]
async fn scope_without_run_id_and_unscoped_routes_admit_target_without_run_capability() {
    let mut rig = rig_with_scope_capabilities(false, false).await;
    rig.sync(vec![session(1)]).await;
    let route = rig
        .bound(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
        .await;
    assert!(rig.live(&route));
    assert_eq!(route.stamp().unwrap().attributes.run_id, None);
    assert!(rig.bound(Some(AFT), PLEXUS, None).await.stamp().is_none());
}

#[tokio::test]
async fn run_scope_rechecks_relay_target_after_reconnect_and_releases_reservation() {
    use std::future::Future;
    let mut rig = rig_with_scope_capabilities(false, true).await;
    rig.sync(vec![run_scope()]).await;
    let (client, mut client_rx, frame) =
        rig.open_frame(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))));
    // A full response queue pauses route.open after admission, before it captures
    // the target connection. Replace the run-capable target during that pause.
    for _ in 0..64 {
        client.egress.try_send(route_bind_ack(1)).unwrap();
    }
    let handler = rig.handler.clone();
    let mut open = Box::pin(handler.handle_control_frame(&client, frame));
    std::future::poll_fn(|cx| {
        assert!(open.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    assert_eq!(rig.forwarding.reserved_route_count().unwrap(), (0, 0));
    rig.handler
        .cleanup_connection(rig.modules[PLEXUS].0.connection_id)
        .unwrap();
    let (replacement, mut replacement_rx) = wide_ctx(200);
    hello_via_sink(
        &rig.handler,
        &replacement,
        &mut replacement_rx,
        hello_frame(PLEXUS, PROTOCOL_VERSION, 200),
    )
    .await;
    client_rx.try_recv().unwrap();
    let replies = tokio::time::timeout(Duration::from_secs(2), open)
        .await
        .expect("the replacement is refused without awaiting a bind ack")
        .unwrap();
    assert_eq!(replies.len(), 1);
    assert_eq!(
        parse_error(&replies[0])["code"],
        error_codes::TARGET_AGENT_RUN_UNSUPPORTED
    );
    assert!(
        replacement_rx.try_recv().is_err(),
        "no run bind is relayed to the replacement"
    );
    assert!(rig.modules.get_mut(PLEXUS).unwrap().1.try_recv().is_err());
    assert_eq!(rig.forwarding.reserved_route_count().unwrap(), (0, 0));
}

#[tokio::test(start_paused = true)]
async fn scope_expiry_loop_sweeps_within_one_second_using_injected_wall_clock() {
    let mut rig = rig().await;
    let clock = set_clock(&mut rig);
    rig.sync(vec![session(1).with_expires_at_ms(Some(2_000))])
        .await;
    let mut route = rig
        .bound(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
        .await;
    Arc::new(rig.handler.clone()).spawn_scope_expiry_loop();
    tokio::task::yield_now().await;
    assert!(rig.live(&route));
    // Advancing Tokio's timer alone does not move the separately injected wall
    // clock past the deadline, so the scope must survive this tick.
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert!(rig.live(&route));
    clock.store(2_000, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert!(!rig.live(&route));
    assert_eq!(route.closed_reason(), RouteCloseReason::ScopeEnded);
}

/// The loop waits without ticking while no live scope has a deadline. A
/// deadline added after it has gone idle must wake it, or that scope would
/// never expire.
#[tokio::test(start_paused = true)]
async fn idle_scope_expiry_loop_wakes_when_a_deadline_is_added() {
    let mut rig = rig().await;
    let clock = set_clock(&mut rig);
    Arc::new(rig.handler.clone()).spawn_scope_expiry_loop();
    tokio::task::yield_now().await;
    rig.sync(vec![session(1).with_expires_at_ms(Some(2_000))])
        .await;
    let mut route = rig
        .bound(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
        .await;
    clock.store(2_000, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert!(!rig.live(&route));
    assert_eq!(route.closed_reason(), RouteCloseReason::ScopeEnded);
}

#[tokio::test]
async fn scope_workdir_bind_is_exactly_opt_in() {
    for declares in [false, true] {
        let mut rig =
            rig_with_target_capabilities(if declares { &["scope-workdir/v1"] } else { &[] }).await;
        let dir = ScratchDir::new("scope-bind-workdir");
        let workdir = daemon_canonical(dir.path())
            .to_str()
            .unwrap()
            .to_owned();
        rig.sync(vec![head("s", 1)
            .with_workdir(Some(workdir.clone()))
            .with_project_id(Some("project:1".into()))])
            .await;
        let bound = rig
            .bound(Some(OWNER), PLEXUS, Some(rig_selector("s", Some(1))))
            .await;
        let body: Value = serde_json::from_slice(&bound.bind.body).unwrap();
        let identity = format!(
            "{{\"project_root\":{},\"harness\":\"unit\",\"session\":\"session\"}}",
            body["identity"]["project_root"]
        );
        // Only the temporary identity path and allocated handles vary between runs.
        let project_fields = if declares {
            format!(
                ",\"workdir\":{},\"project_id\":\"project:1\"",
                serde_json::to_string(&workdir).unwrap()
            )
        } else {
            String::new()
        };
        let expected_scope = format!("{{\"owner\":{{\"kind\":\"reserved\",\"module_id\":\"{OWNER}\"}},\"ref\":\"s\",\"scope_epoch\":1,\"kind\":\"head\"{project_fields},\"owner_authorized\":true}}");
        let expected = format!("{{\"op\":\"route.bind\",\"route_channel\":{},\"epoch\":{},\"target\":{{\"kind\":\"tool_provider\",\"module_id\":\"{PLEXUS}\"}},\"identity\":{identity},\"principal\":{{\"kind\":\"reserved\",\"module_id\":\"{OWNER}\"}},\"scope\":{expected_scope}}}", body["route_channel"], body["epoch"]);
        assert_eq!(bound.bind.body, expected.as_bytes());
    }
}

#[tokio::test]
async fn scope_workdir_live_roots_include_bound_scope_and_keep_legacy_roots() {
    let mut rig = rig_with_target_capabilities(&[]).await;
    let dir = ScratchDir::new("scope-live-root");
    let workdir = daemon_canonical(dir.path());
    rig.sync(vec![
        session(1).with_workdir(Some(workdir.to_str().unwrap().into()))
    ])
    .await;
    let bound = rig
        .bound(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
        .await;
    let legacy: Value = serde_json::from_slice(&bound.bind.body).unwrap();
    let legacy = PathBuf::from(legacy["identity"]["project_root"].as_str().unwrap());
    let ModuleControlResponseToModule::LiveRoots {
        roots,
        total_bindings,
        unknown_root_bindings,
    } = rig.forwarding.live_roots(PLEXUS).unwrap()
    else {
        panic!("live roots");
    };
    assert_eq!(total_bindings, 1);
    assert_eq!(unknown_root_bindings, 0);
    assert_eq!(roots.len(), 2);
    for path in [&workdir, &legacy] {
        let root = roots
            .iter()
            .find(|root| &root.project_root == path)
            .unwrap();
        assert_eq!((root.bound, root.pending), (1, 0));
    }
    assert!(bound.stamp().unwrap().workdir.is_none());
    let pending = rig
        .relayed(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
        .await;
    let ModuleControlResponseToModule::LiveRoots {
        roots,
        total_bindings,
        ..
    } = rig.forwarding.live_roots(PLEXUS).unwrap()
    else {
        panic!("live roots");
    };
    assert_eq!(total_bindings, 2);
    assert_eq!(roots.len(), 3);
    let scope_root = roots
        .iter()
        .find(|root| root.project_root == workdir)
        .unwrap();
    assert_eq!((scope_root.bound, scope_root.pending), (1, 0));
    rig.ack(&pending).await;
    assert!(pending.task.await.unwrap().is_empty());
}

#[tokio::test]
async fn scope_workdir_live_roots_deduplicate_equal_bind_roots_and_only_count_bound_workdirs() {
    let mut rig = rig_with_target_capabilities(&[]).await;
    let dir = ScratchDir::new("scope-equal-live-root");
    let workdir = daemon_canonical(dir.path());
    rig.sync(vec![
        session(1).with_workdir(Some(workdir.to_str().unwrap().into()))
    ])
    .await;
    let (client, mut client_rx, mut frame) =
        rig.open_frame(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))));
    let mut body: Value = serde_json::from_slice(&frame.body).unwrap();
    body["identity"]["project_root"] = serde_json::to_value(&workdir).unwrap();
    frame.body = serde_json::to_vec(&body).unwrap();
    let handler = rig.handler.clone();
    let task =
        tokio::spawn(async move { handler.handle_control_frame(&client, frame).await.unwrap() });
    let (module, module_rx) = rig.modules.get_mut(PLEXUS).unwrap();
    let bind = tokio::time::timeout(Duration::from_secs(2), module_rx.recv())
        .await
        .unwrap()
        .unwrap()
        .frame;
    let ModuleControlResponseToModule::LiveRoots {
        roots,
        total_bindings,
        ..
    } = rig.forwarding.live_roots(PLEXUS).unwrap()
    else {
        panic!("live roots");
    };
    assert_eq!(total_bindings, 1);
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].project_root, workdir);
    assert_eq!((roots[0].bound, roots[0].pending), (0, 1));
    rig.handler
        .handle_control_frame(module, route_bind_ack(bind.header.corr))
        .await
        .unwrap();
    assert!(task.await.unwrap().is_empty());
    client_rx.recv().await.unwrap();
    let ModuleControlResponseToModule::LiveRoots {
        roots,
        total_bindings,
        ..
    } = rig.forwarding.live_roots(PLEXUS).unwrap()
    else {
        panic!("live roots");
    };
    assert_eq!(total_bindings, 1);
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].project_root, workdir);
    assert_eq!((roots[0].bound, roots[0].pending), (1, 0));
}

#[cfg(unix)]
#[tokio::test]
async fn scope_workdir_handler_prepares_canonical_records_and_keeps_path_refusals_per_record() {
    let dir = ScratchDir::new("scope-handler-workdir");
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let canonical = std::fs::canonicalize(&real).unwrap();
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let loop_path = dir.path().join("loop");
    std::os::unix::fs::symlink(&loop_path, &loop_path).unwrap();
    let loop_message = ProjectRootId::from_path_allowing_missing(&loop_path)
        .unwrap_err()
        .to_string();
    for delta in [false, true] {
        let mut rig = rig_with_target_capabilities(&["scope-workdir/v1"]).await;
        rig.sync(vec![
            head("held", 1).with_workdir(Some(canonical.to_str().unwrap().into()))
        ])
        .await;
        let records = vec![
            head("held", 2).with_workdir(Some("relative".into())),
            head("loop", 1).with_workdir(Some(loop_path.to_str().unwrap().into())),
            head("s", 1)
                .with_workdir(Some(alias.join("missing").to_str().unwrap().into()))
                .with_project_id(Some("project:1".into())),
        ];
        let reply = if delta {
            serde_json::from_slice(&apply_call(&rig, 2, records, vec![]).await.body).unwrap()
        } else {
            sync(&rig.handler, &rig.owner, 2, records).await.unwrap()
        };
        let (ModuleControlResponseToModule::ScopeSync { results, .. }
        | ModuleControlResponseToModule::ScopeApply { results, .. }) = reply
        else {
            panic!("scope change reply");
        };
        assert_eq!(results.len(), 3);
        assert_eq!(
            results[0].code.as_deref(),
            Some(error_codes::INVALID_WORKDIR)
        );
        assert_eq!(
            results[0].message.as_deref(),
            Some("workdir must be absolute")
        );
        assert_eq!(
            results[1].code.as_deref(),
            Some(error_codes::INVALID_WORKDIR)
        );
        assert_eq!(results[1].message.as_deref(), Some(loop_message.as_str()));
        assert_eq!(results[2].outcome, ScopeRecordOutcome::Created);
        let ModuleControlResponseToModule::ScopeDescribe {
            scope_epoch,
            scope: Some(held),
            ..
        } = describe(&rig.handler, &rig.owner, OWNER, "held").await
        else {
            panic!("held scope");
        };
        assert_eq!(scope_epoch, Some(1));
        assert_eq!(held.workdir.as_deref(), canonical.to_str());
        let bound = rig
            .bound(Some(OWNER), PLEXUS, Some(rig_selector("s", Some(1))))
            .await;
        let stamp = bound.stamp().unwrap();
        assert_eq!(stamp.workdir.as_deref(), canonical.join("missing").to_str());
        assert_eq!(stamp.project_id.as_deref(), Some("project:1"));
    }
}

/// A folder as the daemon stores it: canonical, and on Windows in the plain
/// spelling. `std::fs::canonicalize` returns the verbatim `\\?\` form there,
/// which the daemon never stores (docs/path-canonicalization.md).
fn daemon_canonical(path: &std::path::Path) -> std::path::PathBuf {
    crate::ProjectRootId::from_path_allowing_missing(path)
        .unwrap()
        .as_path()
        .to_path_buf()
}
