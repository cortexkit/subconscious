use super::*;
use crate::operator_confirm::{OperatorLimits, OperatorProvider, OperatorWithdraw, ProviderResult};
use subc_config::{ApplyKind, FieldChange, Layer, RecordedValue};

const APPROVED: &[u8] = b"{\"limit\":1}";

#[derive(Default)]
struct CountPresence(std::sync::atomic::AtomicUsize);

impl OperatorProvider for CountPresence {
    fn config_gate_ready(&self) -> bool {
        true
    }

    fn prompt(
        &self,
        _: &str,
        _: Box<dyn FnOnce(Arc<dyn OperatorWithdraw>) + Send>,
    ) -> ProviderResult {
        self.0.fetch_add(1, Ordering::Relaxed);
        ProviderResult::Declined
    }

    fn prompt_gate(
        &self,
        text: &str,
        withdraw: Box<dyn FnOnce(Arc<dyn OperatorWithdraw>) + Send>,
    ) -> ProviderResult {
        self.prompt(text, withdraw)
    }
}

fn install_presence_counter(fixture: &Fixture) -> Arc<CountPresence> {
    let provider = Arc::<CountPresence>::default();
    fixture.owner.set_os_prompt_policy_for_tests();
    fixture
        .handler
        .forwarding
        .operator_confirms()
        .configure(provider.clone(), OperatorLimits::default());
    provider
}

async fn file_plan(fixture: &mut Fixture) {
    let prepared = fixture
        .owner
        .prepare_pending_settings()
        .await
        .unwrap()
        .unwrap();
    fixture.plan = fixture
        .owner
        .finish_plan(prepared, |old, new| {
            let mut changeset = daemon_review(old, new)?;
            changeset.fields.push(limit_change());
            Ok(changeset)
        })
        .unwrap();
}

fn limit_change() -> FieldChange {
    let mut change = FieldChange::new(
        "a",
        "/limit",
        RecordedValue::new(json!(1)),
        RecordedValue::new(json!(2)),
        ApplyKind::Live,
    );
    change.old_source = Layer::User;
    change.new_source = Layer::User;
    change
}

fn request(
    fixture: &Fixture,
) -> tokio::task::JoinHandle<std::result::Result<ConfigControlResponse, GateError>> {
    request_with_token(fixture, fixture.owner.next_settings_request_order())
}

fn request_with_token(
    fixture: &Fixture,
    token: u64,
) -> tokio::task::JoinHandle<std::result::Result<ConfigControlResponse, GateError>> {
    let handler = fixture.handler.clone();
    let owner = fixture.owner.clone();
    let digest = fixture.plan.digest.clone();
    tokio::spawn(async move {
        handler
            .request_pending_settings_approval(
                owner,
                digest,
                super::super::pending_settings::AutomaticSettingsAttempt {
                    token,
                    feedback: None,
                },
            )
            .await
    })
}

#[tokio::test]
async fn config_pending_settings_read_only_archive_probe_leaves_the_digest_pending() {
    let fixture = Fixture::new().await;
    let path = fixture.scratch.0.join("cortexkit/a.jsonc");
    std::fs::remove_file(&path).unwrap();
    let plan = fixture
        .owner
        .finish_plan(
            fixture.owner.prepare(PlanKind::Change).unwrap(),
            daemon_review,
        )
        .unwrap();
    fixture.owner.refuse_next_recovery_probe_for_test();
    let error = fixture
        .owner
        .revert_capture_plan(plan.digest.clone())
        .await
        .unwrap_err();
    assert_eq!(error.code, "write_back_failed");
    assert!(!path.exists());
    fixture.owner.plan_get(&plan.digest).unwrap();
    fixture
        .owner
        .revert_capture_plan(plan.digest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), APPROVED);
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_partly_superseded_keep_only_restores_owned_files() {
    let fixture = Fixture::build_with_extra_settings(
        false,
        false,
        Some(json!({"limit":{"type":"integer","description":"Limit","default":1}})),
    )
    .await;
    let a = fixture.scratch.0.join("cortexkit/a.jsonc");
    let b = a.with_file_name("b.jsonc");
    std::fs::write(&b, b"{\"limit\":2}").unwrap();
    let old = fixture
        .owner
        .finish_plan(
            fixture
                .owner
                .prepare_pending_settings()
                .await
                .unwrap()
                .unwrap(),
            daemon_review,
        )
        .unwrap();
    fixture.owner.keep_plan_for_test(&old.digest).unwrap();
    std::fs::write(&b, b"{\"limit\":3}").unwrap();
    let (_, observed) = fixture
        .owner
        .pending_settings_fingerprints()
        .await
        .unwrap()
        .unwrap();
    let ready = std::collections::BTreeSet::from([b.clone()]);
    let next = fixture
        .owner
        .finish_plan(
            fixture
                .owner
                .prepare_confirmed_pending_settings(Some(observed.witnesses(&ready)))
                .await
                .unwrap()
                .unwrap(),
            daemon_review,
        )
        .unwrap();
    fixture.owner.keep_plan_for_test(&next.digest).unwrap();
    let refused = fixture
        .owner
        .revert_capture_plan(old.digest)
        .await
        .unwrap_err();
    assert_eq!(refused.code, "settings_restore_superseded");
    let detail = refused.wire_detail.unwrap();
    let files = detail["files"].as_array().unwrap();
    assert!(files
        .iter()
        .any(|file| file["path"] == b.to_str().unwrap()
            && file["code"] == "settings_restore_superseded"));
    assert!(files
        .iter()
        .any(|file| file["path"] == a.to_str().unwrap() && file["status"] == "restored"));
    assert_eq!(std::fs::read(&b).unwrap(), b"{\"limit\":3}");
    assert_eq!(std::fs::read(&a).unwrap(), APPROVED);
    fixture
        .owner
        .revert_capture_plan(next.digest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(b).unwrap(), b"{}");
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_withdrawing_a_decline_during_restore_keeps_its_plan() {
    let mut fixture = Fixture::new().await;
    file_plan(&mut fixture).await;
    let digest = fixture.plan.digest.clone();
    let token = fixture.owner.next_settings_request_order();
    let (feedback, mut replies) = mpsc::unbounded_channel();
    let handler = fixture.handler.clone();
    let owner = fixture.owner.clone();
    let asking = tokio::spawn({
        let digest = digest.clone();
        async move {
            handler
                .request_pending_settings_approval(
                    owner,
                    digest,
                    super::super::pending_settings::AutomaticSettingsAttempt {
                        token,
                        feedback: Some(feedback),
                    },
                )
                .await
        }
    });
    let (corr, card) = fixture.request().await;
    let restore_gate = fixture.handler.supervisor.operation_lock();
    let held = restore_gate.lock().await;
    assert!(matches!(
        replies.recv().await,
        Some(super::super::pending_settings::CardFeedback::Filed)
    ));
    let (accepted, observed) = tokio::sync::oneshot::channel();
    fixture
        .owner
        .set_after_settings_decline_hook_for_test(move || {
            let _ = accepted.send(());
        });
    decline(&fixture, corr, &card);
    tokio::time::timeout(Duration::from_secs(5), observed)
        .await
        .unwrap()
        .unwrap();
    asking.abort();
    assert!(asking.await.unwrap_err().is_cancelled());
    fixture
        .owner
        .withdraw_settings_card(&digest, token, false)
        .unwrap();
    assert_eq!(fixture.owner.plan_get(&digest).unwrap().digest, digest);
    drop(held);
    let path = fixture.scratch.0.join("cortexkit/a.jsonc");
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut poll = tokio::time::interval(Duration::from_millis(20));
        while fixture.owner.plan_get(&digest).is_ok() {
            poll.tick().await;
        }
    })
    .await
    .expect("the accepted decline must complete without another Revert");
    assert_eq!(std::fs::read(path).unwrap(), APPROVED);
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_failed_source_rename_keeps_revert_retryable() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new().await;
    let directory = fixture.scratch.0.join("cortexkit");
    let file = directory.join("a.jsonc");
    let digest = fixture.plan.digest.clone();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o500)).unwrap();
    let result = fixture.owner.revert_capture_plan(digest.clone()).await;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(result.unwrap_err().code, "write_back_failed");
    assert_eq!(fixture.owner.plan_get(&digest).unwrap().digest, digest);
    assert_eq!(std::fs::read(&file).unwrap(), b"{\"limit\":2}");
    fixture
        .owner
        .revert_capture_plan(digest.clone())
        .await
        .unwrap();
    assert_eq!(std::fs::read(file).unwrap(), APPROVED);
    assert_eq!(
        fixture.owner.plan_get(&digest).unwrap_err().code,
        "unknown_plan"
    );
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_full_keep_table_allows_a_fresh_decline_to_restore() {
    use std::os::unix::fs::PermissionsExt;
    let mut fixture = Fixture::new().await;
    fixture.owner.fill_kept_paths_for_test(&fixture.plan.digest);
    file_plan(&mut fixture).await;
    let opened = request(&fixture);
    let (corr, card) = fixture.request().await;
    decline(&fixture, corr, &card);
    assert_eq!(
        opened.await.unwrap().unwrap_err().code,
        "settings_card_declined"
    );
    let path = fixture.scratch.0.join("cortexkit/a.jsonc");
    assert_eq!(std::fs::read(&path).unwrap(), APPROVED);

    std::fs::write(&path, b"{\"limit\":2}").unwrap();
    file_plan(&mut fixture).await;
    let archive = fixture.scratch.0.join("settings-recovery");
    std::fs::set_permissions(&archive, std::fs::Permissions::from_mode(0o500)).unwrap();
    let opened = request(&fixture);
    let (corr, card) = fixture.request().await;
    decline(&fixture, corr, &card);
    let error = opened.await.unwrap().unwrap_err();
    assert_eq!(error.code, "settings_keep_limit");
    assert_eq!(
        error.wire_detail.unwrap()["files"][0]["code"],
        "write_back_failed"
    );
    assert_eq!(std::fs::read(path).unwrap(), b"{\"limit\":2}");
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_successful_review_clears_refusal_without_a_file_edit() {
    let mut fixture = Fixture::new().await;
    Arc::get_mut(&mut fixture.handler)
        .unwrap()
        .config_review_test_reply = Some(subc_protocol::config::ConfigReviewReply::new(
        vec![limit_change()],
        vec![],
    ));
    let before = fixture.owner.polled_status().unwrap();
    let path = fixture.scratch.0.join("cortexkit/a.jsonc");
    let mut error = subc_control::config::ConfigFileError::new(
        "module_review_refused",
        "refused by the reviewer",
    );
    error.path = Some(path.to_string_lossy().into_owned());
    fixture
        .owner
        .set_settings_review_errors(
            before.base_revision,
            vec![subc_control::config::ConfigStatusFileError::new(
                "a".into(),
                error,
            )],
        )
        .unwrap();
    assert_eq!(fixture.owner.polled_status().unwrap().file_errors.len(), 1);
    let prepared = fixture
        .owner
        .prepare_pending_settings()
        .await
        .unwrap()
        .unwrap();
    fixture
        .handler
        .review_config_plan(fixture.owner.clone(), prepared)
        .await
        .unwrap();
    let after = fixture.owner.polled_status().unwrap();
    assert_eq!(after.base_revision, before.base_revision);
    assert!(after.file_errors.is_empty());
    assert_eq!(std::fs::read(path).unwrap(), b"{\"limit\":2}");
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_mixed_review_clears_only_the_accepted_paths() {
    let mut fixture = Fixture::build_with_extra_settings(
        false,
        false,
        Some(json!({"limit":{"type":"integer","description":"Limit","default":1}})),
    )
    .await;
    let directory = fixture.scratch.0.join("cortexkit");
    std::fs::write(directory.join("b.jsonc"), b"{\"limit\":2}").unwrap();
    let handler = Arc::get_mut(&mut fixture.handler).unwrap();
    let mut b_change = limit_change();
    b_change.module_id = "b".into();
    b_change.old_source = Layer::Default;
    handler.config_review_test_replies.insert(
        "a".into(),
        subc_protocol::config::ConfigReviewReply::new(vec![limit_change()], vec![]),
    );
    handler.config_review_test_replies.insert(
        "b".into(),
        subc_protocol::config::ConfigReviewReply::new(vec![b_change], vec![]),
    );
    for (accepted, refused) in [("a", "b"), ("b", "a")] {
        let base = fixture.owner.polled_status().unwrap().base_revision;
        let rows = ["a", "b"]
            .into_iter()
            .map(|module| {
                let mut error = subc_control::config::ConfigFileError::new(
                    "module_review_refused",
                    "previous refusal",
                );
                error.path = Some(
                    directory
                        .join(format!("{module}.jsonc"))
                        .to_string_lossy()
                        .into_owned(),
                );
                subc_control::config::ConfigStatusFileError::new(module.into(), error)
            })
            .collect();
        fixture
            .owner
            .set_settings_review_errors(base, rows)
            .unwrap();
        *fixture.handler.config_review_test_refuse.lock().unwrap() = [refused.to_owned()].into();
        let prepared = fixture
            .owner
            .prepare_pending_settings()
            .await
            .unwrap()
            .unwrap();
        let refusal = fixture
            .handler
            .review_config_plan(fixture.owner.clone(), prepared)
            .await
            .unwrap_err();
        assert_eq!(refusal.code, "module_review_refused");
        let status = fixture.owner.polled_status().unwrap();
        assert_eq!(status.base_revision, base);
        assert_eq!(
            status
                .file_errors
                .iter()
                .map(|row| row.module_id.as_str())
                .collect::<Vec<_>>(),
            [refused]
        );
        assert_eq!(
            std::fs::read(directory.join(format!("{accepted}.jsonc"))).unwrap(),
            b"{\"limit\":2}"
        );
    }
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_worker_reads_accepted_decline_when_the_poll_wins() {
    let mut fixture = Fixture::build_with_extra_settings(
        false,
        false,
        Some(json!({"limit":{"type":"integer","description":"Limit","default":1}})),
    )
    .await;
    let mut change = limit_change();
    change.module_id = "b".into();
    change.old_source = Layer::Default;
    let handler = Arc::get_mut(&mut fixture.handler).unwrap();
    handler.config_review_test_replies.insert(
        "a".into(),
        subc_protocol::config::ConfigReviewReply::new(vec![limit_change()], vec![]),
    );
    handler.config_review_test_replies.insert(
        "b".into(),
        subc_protocol::config::ConfigReviewReply::new(vec![change], vec![]),
    );
    let b = fixture.scratch.0.join("cortexkit/b.jsonc");
    std::fs::write(&b, b"{\"limit\":2}").unwrap();
    tokio::time::pause();
    let worker = fixture.handler.spawn_pending_settings_worker().unwrap();
    let mut outbound = tokio::time::timeout(Duration::from_secs(90), fixture.frames.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outbound.frame.header.ty, FrameType::Request);
    let corr = outbound.frame.header.corr;
    let card: ConsentRequest = serde_json::from_slice(&outbound.frame.body).unwrap();
    if let Some(flushed) = outbound.flushed.take() {
        let _ = flushed.send(());
    }
    assert_eq!(card.plan.changeset.fields.len(), 2);
    let operation = fixture.handler.supervisor.operation_lock();
    let held = operation.lock().await;
    let (accepted, observed) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    fixture
        .owner
        .set_after_settings_decline_hook_for_test(move || {
            let _ = accepted.send(());
            let _ = wait.recv_timeout(Duration::from_secs(20));
        });
    decline(&fixture, corr, &card);
    observed.await.unwrap();
    std::fs::write(&b, b"{\"limit\":3}").unwrap();
    // A blocked spawn_blocking task suppresses Tokio's automatic time advance.
    // Resume the clock only until the independent file poll reaches cleanup.
    tokio::time::resume();
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut clock = tokio::time::interval(Duration::from_secs(2));
        while fixture
            .owner
            .has_automatic_reservation_for_test(&card.plan_digest)
        {
            clock.tick().await;
        }
    })
    .await
    .expect("the file poll must withdraw while the accepted decision is held");
    release.send(()).unwrap();
    drop(held);
    tokio::time::pause();
    let a = b.with_file_name("a.jsonc");
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut poll = tokio::time::interval(Duration::from_millis(20));
        while std::fs::read(&a).ok().as_deref() != Some(APPROVED) {
            poll.tick().await;
        }
    })
    .await
    .expect("accepted decline must restore A even when the worker poll wins");
    assert_eq!(std::fs::read(&b).unwrap(), b"{\"limit\":3}");
    assert!(
        tokio::time::timeout(Duration::from_secs(35), fixture.frames.recv())
            .await
            .is_err(),
        "the unchanged A must not get a second card"
    );
    assert!(fixture.owner.plan_get(&card.plan_digest).is_ok());
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    tokio::time::resume();
    fixture.stop().await;
}

fn decline(fixture: &Fixture, corr: u64, request: &ConsentRequest) {
    let body =
        serde_json::to_vec(&ConsentResponse::new(request, ConsentDecision::Declined)).unwrap();
    let frame = Frame::build(FrameType::Response, control_flags(), 0, 0, corr, body).unwrap();
    assert!(fixture
        .handler
        .handle_module_relay_response(PHONE, frame)
        .unwrap()
        .is_empty());
}

fn restore_phone_surface(fixture: &Fixture) {
    let mut manifest =
        subc_protocol::manifest::ModuleManifest::builder("callosum", "1.0.0").build();
    manifest.ready = Some(true);
    fixture
        .handler
        .registry
        .register_unpublished_verified_with_control_ops(
            manifest,
            subc_protocol::PROTOCOL_VERSION,
            PHONE,
            vec![subc_protocol::consent::CONSENT_V1.into()],
            true,
        )
        .unwrap();
    fixture
        .handler
        .registry
        .publish_registration(PHONE)
        .unwrap();
}

async fn wait_for_card_reason(fixture: &Fixture, reason: &str) {
    tokio::time::timeout(Duration::from_secs(100), async {
        let mut poll = tokio::time::interval(Duration::from_secs(2));
        while fixture
            .owner
            .pending_settings_card_reason()
            .unwrap()
            .as_deref()
            != Some(reason)
        {
            poll.tick().await;
        }
    })
    .await
    .expect("worker did not publish the expected card reason");
}

#[tokio::test]
async fn config_pending_settings_refused_b_does_not_lose_waiting_a() {
    let mut fixture = Fixture::build_with_extra_settings(
        false,
        false,
        Some(json!({"limit":{"type":"integer","description":"Limit","default":1}})),
    )
    .await;
    let mut change = limit_change();
    change.module_id = "b".into();
    change.old_source = Layer::Default;
    let handler = Arc::get_mut(&mut fixture.handler).unwrap();
    handler.config_review_test_replies.insert(
        "a".into(),
        subc_protocol::config::ConfigReviewReply::new(vec![limit_change()], vec![]),
    );
    handler.config_review_test_replies.insert(
        "b".into(),
        subc_protocol::config::ConfigReviewReply::new(vec![change], vec![]),
    );
    std::fs::write(
        fixture.scratch.0.join("cortexkit/b.jsonc"),
        b"{\"limit\":2}",
    )
    .unwrap();
    fixture
        .handler
        .registry
        .deregister_connection(PHONE)
        .unwrap();
    tokio::time::pause();
    let worker = fixture.handler.spawn_pending_settings_worker().unwrap();
    wait_for_card_reason(&fixture, "no approving device to send the card to").await;
    fixture
        .handler
        .config_review_test_refuse
        .lock()
        .unwrap()
        .insert("b".into());
    restore_phone_surface(&fixture);
    let (corr, card, _) = fixture.next_phone_request().await;
    assert_eq!(card.plan.changeset.fields.len(), 1);
    assert_eq!(card.plan.changeset.fields[0].module_id, "a");
    let file_errors = fixture.owner.polled_status().unwrap().file_errors;
    assert!(file_errors
        .iter()
        .any(|row| row.module_id == "b" && row.error.code == "module_review_refused"));
    assert!(!file_errors.iter().any(|row| row.module_id == "a"));
    fixture.answer(PHONE, corr, &card);
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    tokio::time::resume();
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_waiting_card_is_not_absorbed_into_an_overlarge_batch() {
    let properties = (0..64)
        .map(|index| {
            (
                format!("f{index}"),
                json!({"type":"integer", "description":"Field", "default":1}),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let values = properties
        .keys()
        .map(|key| (key.clone(), json!(2)))
        .collect::<serde_json::Map<_, _>>();
    let fields: Vec<_> = properties
        .keys()
        .map(|key| {
            let mut field = FieldChange::new(
                "b",
                format!("/{key}"),
                RecordedValue::new(json!(1)),
                RecordedValue::new(json!(2)),
                ApplyKind::Live,
            );
            field.old_source = Layer::Default;
            field.new_source = Layer::User;
            field
        })
        .collect();
    let mut fixture =
        Fixture::build_with_extra_settings(false, false, Some(properties.into())).await;
    let (send, mut reviewed) = mpsc::unbounded_channel();
    let handler = Arc::get_mut(&mut fixture.handler).unwrap();
    handler.config_review_test_replies.insert(
        "a".into(),
        subc_protocol::config::ConfigReviewReply::new(vec![limit_change()], vec![]),
    );
    handler.config_review_test_replies.insert(
        "b".into(),
        subc_protocol::config::ConfigReviewReply::new(fields, vec![]),
    );
    handler.config_review_test_seen = Some(send);
    fixture
        .handler
        .registry
        .deregister_connection(PHONE)
        .unwrap();
    tokio::time::pause();
    let worker = fixture.handler.spawn_pending_settings_worker().unwrap();
    wait_for_card_reason(&fixture, "no approving device to send the card to").await;
    assert_eq!(reviewed.try_recv().unwrap(), "a");
    let b = fixture.scratch.0.join("cortexkit/b.jsonc");
    std::fs::write(&b, serde_json::to_vec(&values).unwrap()).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(90), reviewed.recv())
            .await
            .unwrap()
            .unwrap(),
        "b",
        "B's quiet review must not reselect A"
    );
    assert!(fixture.frames.try_recv().is_err());
    restore_phone_surface(&fixture);
    let (corr, a, _) = fixture.next_phone_request().await;
    assert_eq!(a.plan.changeset.fields.len(), 1);
    assert_eq!(a.plan.changeset.fields[0].module_id, "a");
    fixture.answer(PHONE, corr, &a);
    let (_, b, _) = fixture.next_phone_request().await;
    assert_eq!(b.plan.changeset.fields.len(), 64);
    assert!(b
        .plan
        .changeset
        .fields
        .iter()
        .all(|field| field.module_id == "b"));
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    tokio::time::resume();
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_phone_approve_applies_only_the_user_file() {
    let mut fixture = Fixture::new().await;
    let daemon = fixture.scratch.0.join("subc.jsonc");
    let mut raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&daemon).unwrap()).unwrap();
    raw["modules"]["a"]["args"] = json!(["unrelated"]);
    std::fs::write(&daemon, serde_json::to_vec(&raw).unwrap()).unwrap();
    let explicit = fixture
        .owner
        .finish_plan(
            fixture.owner.prepare(PlanKind::Change).unwrap(),
            |old, new| {
                let mut changeset = daemon_review(old, new)?;
                changeset.fields.push(limit_change());
                Ok(changeset)
            },
        )
        .unwrap();
    assert!(!explicit.plan.changeset.launch_manifests.is_empty());
    fixture.plan = explicit;
    let explicit_apply = fixture.apply();
    let (_, _, explicit_wire) = fixture.request_with_wire().await;
    explicit_apply.abort();
    assert!(explicit_apply.await.unwrap_err().is_cancelled());
    assert_eq!(
        fixture.frames.recv().await.unwrap().frame.header.ty,
        FrameType::Cancel
    );
    file_plan(&mut fixture).await;
    assert!(fixture.plan.plan.changeset.launch_manifests.is_empty());
    let display = fixture.plan.settings_file_change.as_ref().unwrap();
    assert_eq!(display.label, "A settings file changed");
    assert_eq!(display.files.len(), 1);
    assert_eq!(
        display.files[0].module_id,
        fixture.plan.plan.changeset.fields[0].module_id
    );
    assert_eq!(
        display.files[0].layer,
        fixture.plan.plan.changeset.fields[0].new_source
    );
    assert_eq!(
        fixture.owner.plan_get(&fixture.plan.digest).unwrap(),
        fixture.plan
    );
    let apply = request(&fixture);
    let (corr, card, automatic_wire) = fixture.request_with_wire().await;
    assert_eq!(
        fixture.owner.pending_settings_card().unwrap(),
        Some(fixture.plan.digest.clone())
    );
    let automatic = serde_json::from_slice(&automatic_wire).unwrap();
    let explicit_body = serde_json::from_slice(&explicit_wire).unwrap();
    for path in [
        "",
        "/plan",
        "/plan/changeset",
        "/plan/changeset/apply_plan",
        "/plan/changeset/fields/0",
    ] {
        let keys = |value: &serde_json::Value| {
            value
                .pointer(path)
                .unwrap()
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
        };
        assert_eq!(
            keys(&automatic),
            keys(&explicit_body),
            "plan body keys at {path}"
        );
    }
    assert!(!std::str::from_utf8(&automatic_wire)
        .unwrap()
        .contains("A settings file changed"));
    fixture.answer(PHONE, corr, &card);
    assert!(matches!(
        apply.await.unwrap().unwrap(),
        ConfigControlResponse::Applied { .. }
    ));
    assert!(fixture.owner.pending_settings_card().unwrap().is_none());
    let manual = fixture
        .owner
        .finish_plan(
            fixture.owner.prepare(PlanKind::Change).unwrap(),
            daemon_review,
        )
        .unwrap();
    assert!(!manual.plan.changeset.launch_manifests.is_empty());
    assert_eq!(
        fixture
            .owner
            .plan_get(&fixture.plan.digest)
            .unwrap_err()
            .code,
        "unknown_plan"
    );
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_decline_restores_exact_bytes() {
    let mut fixture = Fixture::new().await;
    file_plan(&mut fixture).await;
    let apply = request(&fixture);
    let (corr, card) = fixture.request().await;
    decline(&fixture, corr, &card);
    assert_eq!(
        apply.await.unwrap().unwrap_err().code,
        "settings_card_declined"
    );
    assert_eq!(
        std::fs::read(fixture.scratch.0.join("cortexkit/a.jsonc")).unwrap(),
        APPROVED
    );
    assert!(fixture.owner.pending_settings_card().unwrap().is_none());
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_decline_leaves_a_newer_edit_alone() {
    let mut fixture = Fixture::new().await;
    file_plan(&mut fixture).await;
    let apply = request(&fixture);
    let (corr, card) = fixture.request().await;
    let path = fixture.scratch.0.join("cortexkit/a.jsonc");
    std::fs::write(&path, b"{\"limit\":3}").unwrap();
    decline(&fixture, corr, &card);
    assert_eq!(
        apply.await.unwrap().unwrap_err().code,
        "settings_card_decline_unrestored"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"{\"limit\":3}");
    assert!(fixture.owner.plan_get(&card.plan_digest).is_ok());
    assert!(fixture
        .owner
        .prepare_pending_settings()
        .await
        .unwrap()
        .is_some());
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_revert_withdraws_an_open_card() {
    let mut fixture = Fixture::new().await;
    file_plan(&mut fixture).await;
    let apply = request(&fixture);
    let (corr, _) = fixture.request().await;
    fixture
        .handler
        .revert_config_plan(&fixture.owner, fixture.plan.digest.clone())
        .await
        .unwrap();
    let next = tokio::time::timeout(Duration::from_secs(2), fixture.frames.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.frame.header.ty, FrameType::Cancel);
    assert_eq!(next.frame.header.corr, corr);
    assert!(apply.await.unwrap().is_err());
    assert_eq!(
        std::fs::read(fixture.scratch.0.join("cortexkit/a.jsonc")).unwrap(),
        APPROVED
    );
    assert_eq!(
        fixture
            .handler
            .revert_config_plan(&fixture.owner, fixture.plan.digest.clone())
            .await
            .unwrap_err()
            .code,
        "unknown_plan"
    );
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_revert_refuses_a_file_with_a_new_identity() {
    let fixture = Fixture::new().await;
    let path = fixture.scratch.0.join("cortexkit/a.jsonc");
    let replacement = path.with_extension("replacement");
    let proposed = std::fs::read(&path).unwrap();
    std::fs::write(&replacement, &proposed).unwrap();
    std::fs::rename(replacement, &path).unwrap();
    let error = fixture
        .handler
        .revert_config_plan(&fixture.owner, fixture.plan.digest.clone())
        .await
        .unwrap_err();
    assert_eq!(error.code, "config_changed_since_review");
    assert_eq!(std::fs::read(path).unwrap(), proposed);
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_formatting_and_invalid_files_do_not_produce_plans() {
    let fixture = Fixture::new().await;
    let path = fixture.scratch.0.join("cortexkit/a.jsonc");
    for bytes in [
        b"{ \"limit\": 1 }".as_slice(),
        b"{ \"limit\": false }",
        b"{",
    ] {
        std::fs::write(&path, bytes).unwrap();
        assert!(fixture
            .owner
            .prepare_pending_settings()
            .await
            .unwrap()
            .is_none());
    }
    std::fs::write(&path, b"{\"limit\":2}").unwrap();
    assert!(fixture
        .owner
        .prepare_pending_settings()
        .await
        .unwrap()
        .is_some());
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_apply_joins_the_open_digest_without_local_presence() {
    let mut fixture = Fixture::new().await;
    file_plan(&mut fixture).await;
    let automatic = request(&fixture);
    let (corr, card) = fixture.request().await;
    let joined = fixture.apply().await.unwrap().unwrap_err();
    assert_eq!(joined.code, "config_approval_pending");
    assert_eq!(
        joined.wire_detail.as_deref(),
        Some(&json!({"digest":fixture.plan.digest}))
    );
    assert!(fixture.frames.try_recv().is_err());
    fixture.answer(PHONE, corr, &card);
    assert!(matches!(
        automatic.await.unwrap().unwrap(),
        ConfigControlResponse::Applied { .. }
    ));
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_cleanup_preserves_an_explicit_takeover() {
    let mut fixture = Fixture::new().await;
    let prepared = fixture
        .owner
        .prepare_pending_settings()
        .await
        .unwrap()
        .unwrap();
    fixture.plan = fixture.owner.finish_plan(prepared, daemon_review).unwrap();
    let token = fixture.owner.next_settings_request_order();
    let automatic = request_with_token(&fixture, token);
    let (corr, _) = fixture.request().await;
    fixture.reject(corr, "approval_surface_unavailable");
    assert_eq!(
        automatic.await.unwrap().unwrap_err().code,
        "approval_surface_unavailable"
    );
    let cancel = tokio::time::timeout(Duration::from_secs(5), fixture.frames.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancel.frame.header.ty, FrameType::Cancel);
    let explicit = fixture.apply();
    let (corr, card) = fixture.request().await;
    fixture
        .owner
        .withdraw_settings_card(&fixture.plan.digest, token, false)
        .unwrap();
    assert_eq!(
        fixture.owner.plan_get(&fixture.plan.digest).unwrap().digest,
        fixture.plan.digest
    );
    fixture.answer(PHONE, corr, &card);
    assert!(matches!(
        explicit.await.unwrap().unwrap(),
        ConfigControlResponse::Applied { .. }
    ));
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_unreserved_cleanup_cannot_discard_a_replaced_explicit_plan() {
    let mut fixture = Fixture::new().await;
    let newer = fixture.plan.clone();
    let prepared = fixture
        .owner
        .prepare_pending_settings()
        .await
        .unwrap()
        .unwrap();
    fixture.plan = fixture.owner.finish_plan(prepared, daemon_review).unwrap();
    let target = fixture.plan.digest.clone();
    let token = fixture.owner.next_settings_request_order();
    // The automatic worker claimed its plan but never opened a card, so
    // there is no old reservation to distinguish it from a later Apply.
    fixture
        .owner
        .claim_automatic_settings_plan(&target, token)
        .unwrap();
    let explicit = fixture.apply();
    let (old_corr, _) = fixture.request().await;
    fixture.plan = newer;
    let next = fixture.apply_with_corr(882);
    let cancel = tokio::time::timeout(Duration::from_secs(5), fixture.frames.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancel.frame.header.ty, FrameType::Cancel);
    assert_eq!(cancel.frame.header.corr, old_corr);
    let (corr, card) = fixture.request().await;
    assert!(explicit.await.unwrap().is_err());
    fixture
        .owner
        .withdraw_settings_card(&target, token, false)
        .unwrap();
    assert_eq!(fixture.owner.plan_get(&target).unwrap().digest, target);
    fixture.answer(PHONE, corr, &card);
    assert!(matches!(
        next.await.unwrap().unwrap(),
        ConfigControlResponse::Applied { .. }
    ));
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_explicit_phone_card_preempts_and_rejects_late_approval() {
    let mut fixture = Fixture::new().await;
    let explicit = fixture.plan.clone();
    file_plan(&mut fixture).await;
    let automatic = request(&fixture);
    let (old_corr, old_card) = fixture.request().await;
    fixture.plan = explicit;
    let manual = fixture.apply();
    let cancel = tokio::time::timeout(Duration::from_secs(5), fixture.frames.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        cancel.frame.header.ty,
        FrameType::Cancel,
        "withdraw before sending another card"
    );
    assert_eq!(cancel.frame.header.corr, old_corr);
    let (new_corr, new_card) = fixture.request().await;
    fixture.answer(PHONE, old_corr, &old_card);
    assert!(automatic.await.unwrap().is_err());
    assert!(
        !manual.is_finished(),
        "old reply cannot approve the new card"
    );
    fixture.answer(PHONE, new_corr, &new_card);
    assert!(matches!(
        manual.await.unwrap().unwrap(),
        ConfigControlResponse::Applied { .. }
    ));
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_local_apply_withdraws_automatic_card() {
    let mut fixture = Fixture::new().await;
    let explicit = fixture
        .owner
        .finish_plan(
            fixture.owner.prepare(PlanKind::Change).unwrap(),
            |old, new| {
                let mut changeset = daemon_review(old, new)?;
                changeset.fields.push(limit_change());
                Ok(changeset)
            },
        )
        .unwrap();
    file_plan(&mut fixture).await;
    let automatic = request(&fixture);
    let (corr, _) = fixture.request().await;
    let presence = install_presence_counter(&fixture);
    fixture.plan = explicit;
    let manual = fixture.apply();
    let cancel = tokio::time::timeout(Duration::from_secs(5), fixture.frames.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cancel.frame.header.ty, FrameType::Cancel);
    assert_eq!(cancel.frame.header.corr, corr);
    assert_eq!(manual.await.unwrap().unwrap_err().code, "approval_declined");
    assert_eq!(presence.0.load(Ordering::Relaxed), 1);
    assert!(automatic.await.unwrap().is_err());
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_automatic_attempt_never_replaces_an_explicit_card() {
    struct PresenceBarrier {
        entered: tokio::sync::oneshot::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }
    struct HeldPresence(std::sync::Mutex<Option<PresenceBarrier>>);
    impl OperatorProvider for HeldPresence {
        fn config_gate_ready(&self) -> bool {
            true
        }
        fn prompt(
            &self,
            _: &str,
            _: Box<dyn FnOnce(Arc<dyn OperatorWithdraw>) + Send>,
        ) -> ProviderResult {
            ProviderResult::Unavailable
        }
        fn prompt_gate(
            &self,
            _: &str,
            _: Box<dyn FnOnce(Arc<dyn OperatorWithdraw>) + Send>,
        ) -> ProviderResult {
            let PresenceBarrier { entered, release } = self.0.lock().unwrap().take().unwrap();
            entered.send(()).unwrap();
            let _ = release.recv();
            ProviderResult::Approved
        }
    }
    let mut fixture = Fixture::new().await;
    fixture.plan = fixture
        .owner
        .finish_plan(
            fixture.owner.prepare(PlanKind::Change).unwrap(),
            |old, new| {
                let mut changeset = daemon_review(old, new)?;
                changeset.fields.push(limit_change());
                Ok(changeset)
            },
        )
        .unwrap();
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    fixture.owner.set_os_prompt_policy_for_tests();
    fixture.handler.forwarding.operator_confirms().configure(
        Arc::new(HeldPresence(std::sync::Mutex::new(Some(PresenceBarrier {
            entered,
            release: wait,
        })))),
        OperatorLimits::default(),
    );
    let manual = fixture.apply();
    started.await.unwrap();
    file_plan(&mut fixture).await;
    let automatic = request(&fixture);
    let response = tokio::select! {
        result = automatic => result.unwrap(),
        frame = fixture.frames.recv() => panic!("an automatic phone card overtook local approval: {:?}", frame.map(|entry| entry.frame.header.ty)),
    };
    assert_eq!(response.unwrap_err().code, "config_busy");
    release.send(()).unwrap();
    assert!(matches!(
        manual.await.unwrap().unwrap(),
        ConfigControlResponse::Applied { .. }
    ));
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_delayed_explicit_request_cannot_overtake_newer_action() {
    let mut fixture = Fixture::new().await;
    let prepared = fixture
        .owner
        .prepare_pending_settings()
        .await
        .unwrap()
        .unwrap();
    let later_plan = fixture.owner.finish_plan(prepared, daemon_review).unwrap();
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    fixture.owner.set_begin_approval_hook(move || {
        entered.send(()).unwrap();
        let _ = wait.recv();
    });
    let older = fixture.apply();
    started.await.unwrap();
    fixture.plan = later_plan;
    let newer = fixture.apply_with_corr(882);
    let (corr, card) = fixture.request().await;
    release.send(()).unwrap();
    let rejection = tokio::select! {
        result = older => result.unwrap().unwrap_err(),
        event = fixture.frames.recv() => panic!("older request replaced newer card: {:?}", event.map(|event| event.frame.header.ty)),
    };
    assert_eq!(
        rejection.detail,
        "a newer settings approval took precedence"
    );
    assert!(fixture.frames.try_recv().is_err());
    fixture.answer(PHONE, corr, &card);
    assert!(matches!(
        newer.await.unwrap().unwrap(),
        ConfigControlResponse::Applied { .. }
    ));
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_restore_reports_a_save_after_comparison() {
    let fixture = Fixture::new().await;
    fixture
        .owner
        .set_user_restore_hook_for_test(Arc::new(|path| {
            assert!(!path.exists(), "proposal must move aside before comparison");
            std::fs::write(path, b"{\"limit\":3}").unwrap();
            Ok(())
        }));
    let (tx, _rx) = mpsc::channel(1);
    let ctx = RouteCtx {
        connection_id: CALLER,
        egress: crate::FrameSink::new(tx),
    };
    let request = ConfigControlRequest::Revert {
        digest: fixture.plan.digest.clone(),
    };
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        42,
        serde_json::to_vec(&request).unwrap(),
    )
    .unwrap();
    let reply = fixture
        .handler
        .handle_config_gate_request(&ctx, frame, "config.revert")
        .await
        .unwrap()
        .remove(0);
    let body: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(reply.header.ty, FrameType::Error);
    assert_eq!(body["code"], "config_changed_since_review");
    let copy = body["detail"]["recovery_path"].as_str().unwrap();
    assert_eq!(
        std::path::Path::new(copy)
            .parent()
            .unwrap()
            .file_name()
            .unwrap(),
        "settings-recovery"
    );
    let path = fixture.scratch.0.join("cortexkit/a.jsonc");
    assert_eq!(std::fs::read(&path).unwrap(), b"{\"limit\":3}");
    assert_eq!(std::fs::read(copy).unwrap(), b"{\"limit\":2}");
    let status = fixture.owner.polled_status().unwrap();
    assert_eq!(status.settings_recovery_copies.len(), 1);
    assert_eq!(
        status.settings_recovery_copies[0].path,
        path.to_str().unwrap()
    );
    assert_eq!(status.settings_recovery_copies[0].recovery_path, copy);
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_archive_keeps_in_place_save_after_last_read() {
    use std::io::Write;
    let fixture = Fixture::new().await;
    let path = fixture.scratch.0.join("cortexkit/a.jsonc");
    let editor =
        std::sync::Mutex::new(std::fs::OpenOptions::new().write(true).open(&path).unwrap());
    fixture
        .owner
        .set_after_restore_read_hook_for_test(Arc::new(move |_| {
            editor.lock().unwrap().write_all(b"{\"limit\":9}").unwrap();
            Ok(())
        }));
    fixture
        .owner
        .revert_capture_plan(fixture.plan.digest.clone())
        .await
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), APPROVED);
    let archive = std::fs::read_dir(fixture.scratch.0.join("settings-recovery"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| !path.file_name().unwrap().to_string_lossy().starts_with('.'))
        .collect::<Vec<_>>();
    assert_eq!(archive.len(), 1);
    assert!(archive[0]
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .ends_with("-user-a.jsonc"));
    assert_eq!(std::fs::read(&archive[0]).unwrap(), b"{\"limit\":9}");
    assert_eq!(
        fixture
            .owner
            .polled_status()
            .unwrap()
            .settings_recovery_copy_count,
        Some(1)
    );
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_archive_preflight_preserves_the_plan_for_retry() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new().await;
    let archive = fixture.scratch.0.join("settings-recovery");
    std::fs::create_dir(&archive).unwrap();
    std::fs::set_permissions(&archive, std::fs::Permissions::from_mode(0o500)).unwrap();
    let digest = fixture.plan.digest.clone();
    assert_eq!(
        fixture
            .owner
            .revert_capture_plan(digest.clone())
            .await
            .unwrap_err()
            .code,
        "write_back_failed"
    );
    assert_eq!(fixture.owner.plan_get(&digest).unwrap().digest, digest);
    let path = fixture.scratch.0.join("cortexkit/a.jsonc");
    assert_eq!(std::fs::read(&path).unwrap(), b"{\"limit\":2}");
    std::fs::set_permissions(&archive, std::fs::Permissions::from_mode(0o700)).unwrap();
    fixture.owner.revert_capture_plan(digest).await.unwrap();
    assert_eq!(std::fs::read(path).unwrap(), APPROVED);
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_deletion_only_revert_checks_the_archive_first() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new().await;
    let path = fixture.scratch.0.join("cortexkit/a.jsonc");
    std::fs::remove_file(&path).unwrap();
    let plan = fixture
        .owner
        .finish_plan(
            fixture.owner.prepare(PlanKind::Change).unwrap(),
            daemon_review,
        )
        .unwrap();
    let archive = fixture.scratch.0.join("settings-recovery");
    std::fs::create_dir(&archive).unwrap();
    std::fs::set_permissions(&archive, std::fs::Permissions::from_mode(0o500)).unwrap();
    assert_eq!(
        fixture
            .owner
            .revert_capture_plan(plan.digest.clone())
            .await
            .unwrap_err()
            .code,
        "write_back_failed"
    );
    assert!(!path.exists());
    fixture.owner.plan_get(&plan.digest).unwrap();
    std::fs::set_permissions(&archive, std::fs::Permissions::from_mode(0o700)).unwrap();
    let files = fixture
        .owner
        .revert_capture_plan(plan.digest)
        .await
        .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].status,
        subc_control::config::ConfigRevertFileStatus::Restored
    );
    assert_eq!(std::fs::read(path).unwrap(), APPROVED);
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_batched_revert_lists_success_and_each_failure() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::build_with_extra_settings(
        false,
        false,
        Some(json!({"limit":{"type":"integer","description":"Limit","default":1}})),
    )
    .await;
    let dir = fixture.scratch.0.join("cortexkit");
    let a = dir.join("a.jsonc");
    let b = dir.join("b.jsonc");
    let c = dir.join("callosum.jsonc");
    std::fs::write(&b, b"{\"limit\":2}").unwrap();
    std::fs::write(&c, b"{}").unwrap();
    let plan = fixture
        .owner
        .finish_plan(
            fixture.owner.prepare(PlanKind::Change).unwrap(),
            daemon_review,
        )
        .unwrap();
    std::fs::write(&a, b"{\"limit\":3}").unwrap();
    std::fs::write(&c, b"{ }\n").unwrap();
    let error = fixture
        .owner
        .revert_capture_plan(plan.digest.clone())
        .await
        .unwrap_err();
    assert_eq!(error.code, "config_changed_since_review");
    let detail = error.wire_detail.unwrap();
    let files: Vec<subc_control::config::ConfigRevertFile> =
        serde_json::from_value(detail["files"].clone()).unwrap();
    use subc_control::config::ConfigRevertFileStatus::{Refused, Restored};
    assert_eq!(
        files
            .iter()
            .map(|file| (file.path.as_str(), file.status))
            .collect::<Vec<_>>(),
        [
            (a.to_str().unwrap(), Refused),
            (b.to_str().unwrap(), Restored),
            (c.to_str().unwrap(), Refused),
        ]
    );
    for file in [&files[0], &files[2]] {
        assert_eq!(file.code.as_deref(), Some("config_changed_since_review"));
        assert!(file.message.is_some());
    }
    assert_eq!(std::fs::read(b).unwrap(), b"{}");
    assert_eq!(std::fs::read(a).unwrap(), b"{\"limit\":3}");
    assert_eq!(std::fs::read(c).unwrap(), b"{ }\n");
    let archive = fixture.scratch.0.join("settings-recovery");
    std::fs::set_permissions(&archive, std::fs::Permissions::from_mode(0o500)).unwrap();
    let error = fixture
        .owner
        .revert_capture_plan(plan.digest)
        .await
        .unwrap_err();
    std::fs::set_permissions(archive, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(error.code, "write_back_failed");
    let detail = error.wire_detail.unwrap();
    let retried: Vec<subc_control::config::ConfigRevertFile> =
        serde_json::from_value(detail["files"].clone()).unwrap();
    let already_restored = retried
        .iter()
        .find(|row| row.path == files[1].path)
        .unwrap();
    assert_eq!(already_restored.status, Restored);
    assert!(already_restored.code.is_none());
    assert!(retried
        .iter()
        .filter(|row| row.status == Refused)
        .all(|row| row.code.as_deref() == Some("write_back_failed")));
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_decline_archive_move_failure_is_visible_in_status() {
    use std::os::unix::fs::PermissionsExt;
    let mut fixture = Fixture::new().await;
    file_plan(&mut fixture).await;
    let directory = fixture.scratch.0.join("settings-recovery");
    fixture
        .owner
        .set_after_restore_read_hook_for_test(Arc::new(move |_| {
            // Keep the pre-recorded inventory readable while making the archive
            // unable to accept a move after preflight.
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o500)).unwrap();
            Ok(())
        }));
    let opened = request(&fixture);
    let (corr, card) = fixture.request().await;
    decline(&fixture, corr, &card);
    let refusal = opened.await.unwrap().unwrap_err();
    assert_eq!(refusal.code, "settings_card_decline_unrestored");
    let detail = refusal.wire_detail.unwrap();
    let path = detail["recovery_path"].as_str().unwrap();
    let status = fixture.owner.polled_status().unwrap();
    assert!(status
        .settings_recovery_copies
        .iter()
        .any(|copy| copy.recovery_path == path));
    assert!(std::path::Path::new(path)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with(".a.jsonc.declined-"));
    assert_eq!(std::fs::read(path).unwrap(), b"{\"limit\":2}");
    let archive = fixture.scratch.0.join("settings-recovery");
    std::fs::set_permissions(archive, std::fs::Permissions::from_mode(0o700)).unwrap();
    // The approved bytes were already installed before the archive failed.
    // Retry must recognize that progress rather than compare against old bytes.
    fixture
        .owner
        .revert_capture_plan(card.plan_digest)
        .await
        .unwrap();
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_decline_without_an_archive_leaves_the_same_digest() {
    use std::os::unix::fs::PermissionsExt;
    let mut fixture = Fixture::build_with_extra_settings(
        false,
        false,
        Some(json!({"limit":{"type":"integer","description":"Limit","default":1}})),
    )
    .await;
    let archive = fixture.scratch.0.join("settings-recovery");
    std::fs::create_dir(&archive).unwrap();
    std::fs::set_permissions(&archive, std::fs::Permissions::from_mode(0o500)).unwrap();
    Arc::get_mut(&mut fixture.handler)
        .unwrap()
        .config_review_test_reply = Some(subc_protocol::config::ConfigReviewReply::new(
        vec![limit_change()],
        vec![],
    ));
    tokio::time::pause();
    let worker = fixture.handler.spawn_pending_settings_worker().unwrap();
    let mut outbound = tokio::time::timeout(Duration::from_secs(90), fixture.frames.recv())
        .await
        .unwrap()
        .unwrap();
    let card: ConsentRequest = serde_json::from_slice(&outbound.frame.body).unwrap();
    if let Some(flushed) = outbound.flushed.take() {
        let _ = flushed.send(());
    }
    decline(&fixture, outbound.frame.header.corr, &card);
    tokio::time::timeout(Duration::from_secs(60), async {
        let mut poll = tokio::time::interval(Duration::from_secs(2));
        while fixture
            .owner
            .has_automatic_reservation_for_test(&card.plan_digest)
        {
            poll.tick().await;
        }
    })
    .await
    .expect("the declined card should close");
    std::fs::write(
        fixture.scratch.0.join("cortexkit/b.jsonc"),
        b"{ /* formatting only */ }\n",
    )
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(70), fixture.frames.recv())
            .await
            .is_err()
    );
    assert_eq!(
        fixture.owner.plan_get(&card.plan_digest).unwrap().digest,
        card.plan_digest
    );
    let path = fixture.scratch.0.join("cortexkit/a.jsonc");
    assert_eq!(std::fs::read(&path).unwrap(), b"{\"limit\":2}");
    std::fs::set_permissions(&archive, std::fs::Permissions::from_mode(0o700)).unwrap();
    fixture
        .handler
        .revert_config_plan(&fixture.owner, card.plan_digest)
        .await
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), APPROVED);
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    tokio::time::resume();
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_failed_final_read_is_not_a_newer_edit() {
    let fixture = Fixture::new().await;
    let path = fixture.scratch.0.join("cortexkit/a.jsonc");
    let editor = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    let oversized = 1024 * 1024 + 1;
    fixture
        .owner
        .set_user_restore_hook_for_test(Arc::new(move |_| {
            editor.set_len(oversized).unwrap();
            Ok(())
        }));
    let error = fixture
        .owner
        .revert_capture_plan(fixture.plan.digest.clone())
        .await
        .unwrap_err();
    assert_eq!(error.code, "write_back_failed");
    let detail = error.wire_detail.unwrap();
    assert!(detail.get("recovery_path").is_none());
    assert_eq!(detail["files"][0]["code"], "write_back_failed");
    assert!(fixture
        .owner
        .polled_status()
        .unwrap()
        .settings_recovery_copies
        .is_empty());
    assert_eq!(std::fs::metadata(path).unwrap().len(), oversized);
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_restore_refuses_a_replaced_parent_symlink() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new().await;
    let actual = fixture.scratch.0.join("cortexkit");
    let detached = fixture.scratch.0.join("detached");
    let elsewhere = fixture.scratch.0.join("elsewhere");
    std::fs::rename(&actual, &detached).unwrap();
    std::fs::create_dir(&elsewhere).unwrap();
    let file = elsewhere.join("a.jsonc");
    std::fs::rename(detached.join("a.jsonc"), &file).unwrap();
    symlink(&elsewhere, &actual).unwrap();
    assert_eq!(
        fixture
            .owner
            .revert_capture_plan(fixture.plan.digest.clone())
            .await
            .unwrap_err()
            .code,
        "config_changed_since_review"
    );
    assert_eq!(std::fs::read(file).unwrap(), b"{\"limit\":2}");
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_restore_never_widens_recorded_user_mode() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new().await;
    let path = fixture.scratch.0.join("cortexkit/a.jsonc");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let plan = fixture
        .owner
        .finish_plan(
            fixture.owner.prepare(PlanKind::Change).unwrap(),
            daemon_review,
        )
        .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();
    fixture
        .owner
        .revert_capture_plan(plan.digest)
        .await
        .unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::remove_file(&path).unwrap();
    let sibling = path.with_file_name("sibling.jsonc");
    std::fs::write(&sibling, b"{}").unwrap();
    std::fs::set_permissions(&sibling, std::fs::Permissions::from_mode(0o200)).unwrap();
    let deleted = fixture
        .owner
        .finish_plan(
            fixture.owner.prepare(PlanKind::Change).unwrap(),
            daemon_review,
        )
        .unwrap();
    fixture
        .owner
        .revert_capture_plan(deleted.digest)
        .await
        .unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o200
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), APPROVED);
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_phone_only_never_uses_os_presence() {
    let mut fixture = Fixture::new().await;
    file_plan(&mut fixture).await;
    let presence = install_presence_counter(&fixture);
    let apply = request(&fixture);
    let (corr, card) = fixture.request().await;
    fixture.answer(PHONE, corr, &card);
    assert!(matches!(
        apply.await.unwrap().unwrap(),
        ConfigControlResponse::Applied { .. }
    ));
    assert_eq!(presence.0.load(Ordering::Relaxed), 0);
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_phone_only_waits_for_a_device_without_prompting_or_editing() {
    let mut fixture = Fixture::new().await;
    Arc::get_mut(&mut fixture.handler)
        .unwrap()
        .config_review_test_reply = Some(subc_protocol::config::ConfigReviewReply::new(
        vec![limit_change()],
        vec![],
    ));
    let presence = install_presence_counter(&fixture);
    fixture
        .handler
        .registry
        .deregister_connection(PHONE)
        .unwrap();
    assert!(fixture.handler.config_phone_surface().await.is_none());

    tokio::time::pause();
    let worker = fixture.handler.spawn_pending_settings_worker().unwrap();
    let (tx, _rx) = mpsc::channel(1);
    let ctx = RouteCtx {
        connection_id: CALLER,
        egress: crate::FrameSink::new(tx),
    };
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 42, Vec::new()).unwrap();
    let waiting = tokio::time::timeout(Duration::from_secs(90), async {
        let mut status_poll = tokio::time::interval(Duration::from_secs(2));
        loop {
            status_poll.tick().await;
            let ConfigControlResponse::Status { status } = fixture
                .handler
                .execute_config_request(
                    &ctx,
                    &frame,
                    fixture.owner.clone(),
                    ConfigControlRequest::Status {},
                )
                .await
                .unwrap()
            else {
                panic!("expected config.status")
            };
            if status.pending_settings_card_reason.is_some() {
                break status;
            }
        }
    })
    .await
    .expect("settings status must explain why no card was filed");
    assert_eq!(
        waiting.pending_settings_card_reason.as_deref(),
        Some("no approving device to send the card to")
    );
    assert!(waiting.pending_settings_card.is_none());
    assert!(fixture.frames.try_recv().is_err());
    assert_eq!(presence.0.load(Ordering::Relaxed), 0);
    assert_eq!(
        std::fs::read(fixture.scratch.0.join("cortexkit/a.jsonc")).unwrap(),
        b"{\"limit\":2}"
    );

    let mut manifest =
        subc_protocol::manifest::ModuleManifest::builder("callosum", "1.0.0").build();
    manifest.ready = Some(true);
    fixture
        .handler
        .registry
        .register_unpublished_verified_with_control_ops(
            manifest,
            subc_protocol::PROTOCOL_VERSION,
            PHONE,
            vec![subc_protocol::consent::CONSENT_V1.into()],
            true,
        )
        .unwrap();
    fixture
        .handler
        .registry
        .publish_registration(PHONE)
        .unwrap();
    let outbound = tokio::time::timeout(Duration::from_secs(10), fixture.frames.recv())
        .await
        .expect("a newly available phone must receive the unchanged proposal")
        .unwrap();
    assert_eq!(outbound.frame.header.ty, FrameType::Request);
    let card: ConsentRequest = serde_json::from_slice(&outbound.frame.body).unwrap();
    assert_eq!(
        fixture.owner.pending_settings_card().unwrap(),
        Some(card.plan_digest)
    );
    assert!(fixture
        .owner
        .pending_settings_card_reason()
        .unwrap()
        .is_none());
    assert_eq!(presence.0.load(Ordering::Relaxed), 0);
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    tokio::time::resume();
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_retries_a_reviewer_without_a_new_edit() {
    let mut fixture = Fixture::new().await;
    Arc::get_mut(&mut fixture.handler)
        .unwrap()
        .config_review_test_reply = Some(subc_protocol::config::ConfigReviewReply::new(
        vec![limit_change()],
        vec![],
    ));
    fixture
        .handler
        .config_review_test_fail_once
        .store(true, Ordering::Relaxed);
    tokio::time::pause();
    let started = Instant::now();
    let worker = fixture.handler.spawn_pending_settings_worker().unwrap();
    let outbound = tokio::time::timeout(Duration::from_secs(90), fixture.frames.recv())
        .await
        .expect("the temporarily unavailable reviewer must not strand the proposal")
        .unwrap();
    assert_eq!(outbound.frame.header.ty, FrameType::Request);
    assert!(Instant::now() >= started + Duration::from_secs(60));
    assert!(!fixture
        .handler
        .config_review_test_fail_once
        .load(Ordering::Relaxed));
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    tokio::time::resume();
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_retries_unavailable_answer_on_the_same_phone() {
    let mut fixture = Fixture::new().await;
    Arc::get_mut(&mut fixture.handler)
        .unwrap()
        .config_review_test_reply = Some(subc_protocol::config::ConfigReviewReply::new(
        vec![limit_change()],
        vec![],
    ));
    tokio::time::pause();
    let worker = fixture.handler.spawn_pending_settings_worker().unwrap();
    let mut first = tokio::time::timeout(Duration::from_secs(90), fixture.frames.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.frame.header.ty, FrameType::Request);
    if let Some(flushed) = first.flushed.take() {
        let _ = flushed.send(());
    }
    let rejected_at = Instant::now();
    fixture.reject(first.frame.header.corr, "approval_surface_unavailable");
    let mut next = tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let frame = fixture.frames.recv().await.unwrap();
            if frame.frame.header.ty == FrameType::Request {
                break frame;
            }
        }
    })
    .await
    .expect("an available but temporarily rejecting phone must be retried");
    assert_ne!(next.frame.header.corr, first.frame.header.corr);
    assert!(Instant::now() >= rejected_at + Duration::from_secs(30));
    let before: ConsentRequest = serde_json::from_slice(&first.frame.body).unwrap();
    let after: ConsentRequest = serde_json::from_slice(&next.frame.body).unwrap();
    assert_eq!(before.plan_digest, after.plan_digest);
    if let Some(flushed) = next.flushed.take() {
        let _ = flushed.send(());
    }
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    tokio::time::resume();
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_reviewer_refusal_remains_a_file_error() {
    let mut fixture = Fixture::new().await;
    Arc::get_mut(&mut fixture.handler)
        .unwrap()
        .config_review_test_reply = Some(subc_protocol::config::ConfigReviewReply::new(
        vec![],
        vec![subc_protocol::config::ConfigReviewRefusal::new(
            "out_of_bounds",
            vec!["/limit".into()],
            vec![],
        )],
    ));
    tokio::time::pause();
    let worker = fixture.handler.spawn_pending_settings_worker().unwrap();
    let error = tokio::time::timeout(Duration::from_secs(90), async {
        let mut poll = tokio::time::interval(Duration::from_secs(2));
        loop {
            poll.tick().await;
            if let Some(error) = fixture
                .owner
                .polled_status()
                .unwrap()
                .file_errors
                .into_iter()
                .find(|row| row.error.code == "module_review_refused")
            {
                break error;
            }
        }
    })
    .await
    .expect("the module refusal should be a status file error");
    assert_eq!(error.module_id, "a");
    assert!(error.error.path.unwrap().ends_with("a.jsonc"));
    assert!(
        tokio::time::timeout(Duration::from_secs(600), fixture.frames.recv())
            .await
            .is_err()
    );
    assert!(fixture
        .owner
        .polled_status()
        .unwrap()
        .file_errors
        .iter()
        .any(|row| row.error.code == "module_review_refused"));
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    tokio::time::resume();
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_worker_does_not_create_an_archive_without_restore() {
    let mut fixture = Fixture::new().await;
    Arc::get_mut(&mut fixture.handler)
        .unwrap()
        .config_review_test_reply = Some(subc_protocol::config::ConfigReviewReply::new(
        vec![limit_change()],
        vec![],
    ));
    tokio::time::pause();
    let worker = fixture.handler.spawn_pending_settings_worker().unwrap();
    let outbound = tokio::time::timeout(Duration::from_secs(90), fixture.frames.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outbound.frame.header.ty, FrameType::Request);
    assert!(!fixture.scratch.0.join("settings-recovery").exists());
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    tokio::time::resume();
    fixture.stop().await;
}

#[tokio::test]
async fn config_pending_settings_worker_withdraws_and_refiles_after_an_edit() {
    let mut fixture = Fixture::new().await;
    Arc::get_mut(&mut fixture.handler)
        .unwrap()
        .config_review_test_reply = Some(subc_protocol::config::ConfigReviewReply::new(
        vec![limit_change()],
        vec![],
    ));
    tokio::time::pause();
    let worker = fixture.handler.spawn_pending_settings_worker().unwrap();

    async fn next(
        frames: &mut mpsc::Receiver<crate::router::OutboundFrame>,
        kind: FrameType,
    ) -> Frame {
        let mut outbound = tokio::time::timeout(Duration::from_secs(90), frames.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outbound.frame.header.ty, kind);
        if let Some(flushed) = outbound.flushed.take() {
            let _ = flushed.send(());
        }
        outbound.frame
    }
    let first = next(&mut fixture.frames, FrameType::Request).await;
    let first_plan: ConsentRequest = serde_json::from_slice(&first.body).unwrap();
    std::fs::write(
        fixture.scratch.0.join("cortexkit/a.jsonc"),
        b"{ \"limit\":2 }",
    )
    .unwrap();
    let cancel = next(&mut fixture.frames, FrameType::Cancel).await;
    assert_eq!(cancel.header.corr, first.header.corr);
    let cancelled_at = Instant::now();
    let second = next(&mut fixture.frames, FrameType::Request).await;
    let second_plan: ConsentRequest = serde_json::from_slice(&second.body).unwrap();
    assert_ne!(first_plan.plan_digest, second_plan.plan_digest);
    assert!(Instant::now() >= cancelled_at + Duration::from_secs(30));
    assert_eq!(
        fixture
            .owner
            .plan_get(&first_plan.plan_digest)
            .unwrap_err()
            .code,
        "unknown_plan"
    );
    tokio::time::advance(Duration::from_secs(300)).await;
    let expired = next(&mut fixture.frames, FrameType::Cancel).await;
    assert_eq!(expired.header.corr, second.header.corr);
    assert!(
        tokio::time::timeout(Duration::from_secs(62), fixture.frames.recv())
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(fixture.scratch.0.join("cortexkit/a.jsonc")).unwrap(),
        b"{ \"limit\":2 }"
    );
    assert!(fixture.owner.pending_settings_card().unwrap().is_none());
    std::fs::write(
        fixture.scratch.0.join("cortexkit/a.jsonc"),
        b"{\n\"limit\":2\n}",
    )
    .unwrap();
    let third = next(&mut fixture.frames, FrameType::Request).await;
    let third_plan: ConsentRequest = serde_json::from_slice(&third.body).unwrap();
    assert_ne!(third_plan.plan_digest, second_plan.plan_digest);
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    tokio::time::resume();
    fixture.stop().await;
}
