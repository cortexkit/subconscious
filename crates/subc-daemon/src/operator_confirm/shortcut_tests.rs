use super::*;
use crate::activity::{ActivityLedger, WriterHooks};
use crate::app_open::LaunchTable;
use crate::operator_apps::OperatorAppStamp;
use crate::operator_confirm::shortcut::{Gate, Hooks, ShortcutContext};
use cortexkit_test_support::ScratchDir;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Prompts(Arc<AtomicUsize>);
impl OperatorProvider for Prompts {
    fn prompt(
        &self,
        _: &str,
        _: Box<dyn FnOnce(Arc<dyn OperatorWithdraw>) + Send>,
    ) -> ProviderResult {
        self.0.fetch_add(1, Ordering::SeqCst);
        ProviderResult::Declined
    }
}

struct ShortcutFixture {
    f: Fixture,
    _dir: ScratchDir,
    path: PathBuf,
    context: ShortcutContext,
    prompts: Arc<AtomicUsize>,
    settled: Arc<Notify>,
}

impl ShortcutFixture {
    fn new(hooks: Hooks, writer: WriterHooks) -> Self {
        let dir = ScratchDir::new("shortcut-lifecycle");
        let path = dir.path().join("activity.jsonl");
        let ledger = ActivityLedger::start(path.clone(), "shortcut-test".into(), 64, writer);
        let prompts = Arc::new(AtomicUsize::new(0));
        let f = Fixture::new(
            Arc::new(Prompts(prompts.clone())),
            OperatorLimits::default(),
        );
        let settled = Arc::new(Notify::new());
        let context = ShortcutContext {
            ledger: Some(ledger),
            forwarding: f.forwarding.clone(),
            launches: Arc::new(LaunchTable::new()),
            stamp: None,
            phone_fingerprint: Some("0123456789abcdef0123456789abcdef".into()),
            route: None,
            connections: Arc::default(),
            hooks: Hooks {
                settled: Some(settled.clone()),
                ..hooks
            },
        };
        Self {
            f,
            _dir: dir,
            path,
            context,
            prompts,
            settled,
        }
    }

    async fn start(&self, privileged: bool) -> Running {
        let module = self.f.module("target", 51, "nonce").await;
        let mut context = self.context.clone();
        let confirms = self.f.forwarding.operator_confirms();
        let task = if privileged {
            let ctx = module.ctx.clone();
            context
                .connections
                .lock()
                .unwrap()
                .insert(ctx.connection_id, OperatorAppStamp::fixture("app", 1));
            Some(tokio::spawn(async move {
                confirms
                    .approve_privileged_with_shortcut(
                        &ctx,
                        &frame(
                            FrameType::Request,
                            91,
                            serde_json::json!({"op":"daemon.restart"}),
                        ),
                        "daemon.restart",
                        "secret control body".into(),
                        "direct".into(),
                        DeclineKey::for_caller(&CallerIdentity::default(), ctx.connection_id),
                        Some(context),
                    )
                    .await
            }))
        } else {
            context.route = Some((module.key.channel, module.epoch));
            let answers = self
                .f
                .forwarding
                .with_operator_route(
                    module.ctx.connection_id,
                    module.key.channel,
                    module.epoch,
                    |binding| {
                        confirms.admit_with_shortcut(
                            &module.ctx,
                            frame(FrameType::Request, 91, serde_json::json!({})),
                            "target".into(),
                            true,
                            OperatorConfirmRequest::new(
                                "secret confirm summary",
                                module.key.channel,
                                module.epoch,
                            ),
                            binding,
                            Some(context),
                        )
                    },
                )
                .unwrap();
            assert!(answers.is_empty());
            None
        };
        Running { module, task }
    }

    async fn settled(&self) {
        tokio::time::timeout(Duration::from_secs(4), self.settled.notified())
            .await
            .unwrap();
    }
    fn records(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(&self.path)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn assert_records(&self, expected: &[&str]) {
        let records = self.records();
        assert_eq!(records.len(), expected.len(), "{records:?}");
        let attempt = records.first().map(|r| r["details"]["attempt"].clone());
        for (record, expected) in records.iter().zip(expected) {
            assert_eq!(record["details"]["attempt"], *attempt.as_ref().unwrap());
            assert!(!record.to_string().contains("secret"));
            if *expected == "attempt" {
                assert_eq!(record["kind"], "operator_shortcut");
                assert_eq!(record["summary"], "Operator shortcut attempted");
                assert_eq!(
                    record["details"]["phone_fingerprint"],
                    "0123456789abcdef0123456789abcdef"
                );
                assert!(
                    record["details"]["operation"]
                        .as_str()
                        .unwrap()
                        .starts_with("operator.confirm:")
                        || record["details"]["operation"] == "daemon.restart"
                );
            } else {
                assert_eq!(record["kind"], "operator_shortcut_outcome");
                assert_eq!(record["summary"], "Operator shortcut ended");
                assert_eq!(record["details"]["outcome"], *expected);
            }
        }
    }
}

struct Running {
    module: Module,
    task: Option<tokio::task::JoinHandle<Outcome>>,
}
impl Running {
    async fn answer(&mut self) -> serde_json::Value {
        if let Some(task) = self.task.take() {
            let answer = task.await.unwrap();
            serde_json::json!({"code":answer.code,"detail":{"reason":answer.reason}})
        } else {
            let answer = receive(&mut self.module.rx).await;
            assert_eq!(answer.header.corr, 91);
            if answer.header.ty == FrameType::Response {
                serde_json::json!({"code":"confirmed"})
            } else {
                body(&answer)
            }
        }
    }
    fn assert_pending(&mut self) {
        if let Some(task) = &self.task {
            assert!(!task.is_finished());
        } else {
            assert!(self.module.rx.try_recv().is_err());
        }
    }
}

fn gate() -> Arc<Gate> {
    Arc::default()
}
async fn entered(gate: &Gate) {
    tokio::time::timeout(Duration::from_secs(4), gate.entered.notified())
        .await
        .unwrap();
}

#[tokio::test]
async fn healthy_attempt_commits_before_unprompted_answer_and_has_shared_audit_id() {
    for privileged in [false, true] {
        let hold = gate();
        let f = ShortcutFixture::new(
            Hooks {
                after_append: Some(hold.clone()),
                ..Default::default()
            },
            WriterHooks::default(),
        );
        let mut running = f.start(privileged).await;
        entered(&hold).await;
        running.assert_pending();
        f.assert_records(&["attempt"]);
        hold.release.notify_one();
        assert_eq!(running.answer().await["code"], "confirmed");
        f.settled().await;
        f.assert_records(&["attempt", "confirmed"]);
        assert_eq!(f.prompts.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn withdrawals_during_held_append_answer_first_and_cannot_authorize_later() {
    for privileged in [false, true] {
        for error in ["none", "substituted", "sink-closing"] {
            let hold = gate();
            let f = ShortcutFixture::new(
                Hooks {
                    before_append: Some(hold.clone()),
                    substitute_error: error == "substituted",
                    ..Default::default()
                },
                WriterHooks {
                    write_failure: error == "sink-closing",
                    ..Default::default()
                },
            );
            let mut running = f.start(privileged).await;
            entered(&hold).await;
            assert!(f
                .f
                .handler
                .handle_control_frame(
                    &running.module.ctx,
                    frame(FrameType::Cancel, 91, serde_json::json!({}))
                )
                .await
                .unwrap()
                .is_empty());
            let answer = running.answer().await;
            assert_eq!(answer["code"], error_codes::OPERATOR_DECLINED);
            assert_eq!(answer["detail"]["reason"], "caller_cancelled");
            assert!(f.records().is_empty());
            hold.release.notify_one();
            f.settled().await;
            f.assert_records(if error == "sink-closing" {
                &[]
            } else {
                &["attempt", "withdrawn"]
            });
            assert_eq!(f.prompts.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn route_module_and_connection_close_withdraw_held_shortcuts() {
    for reason in ["route_closed", "module_closed", "connection_closed"] {
        let hold = gate();
        let f = ShortcutFixture::new(
            Hooks {
                before_append: Some(hold.clone()),
                ..Default::default()
            },
            WriterHooks::default(),
        );
        let mut running = f.start(reason == "connection_closed").await;
        entered(&hold).await;
        match reason {
            "route_closed" => f.f.close(&running.module),
            "module_closed" => {
                f.f.handler
                    .cleanup_connection(running.module.ctx.connection_id)
                    .unwrap();
            }
            _ => {
                f.f.forwarding
                    .operator_confirms()
                    .connection_closed(running.module.ctx.connection_id);
            }
        }
        let answer = running.answer().await;
        assert_eq!(
            answer["detail"]["reason"],
            if reason == "connection_closed" {
                "caller_cancelled"
            } else {
                reason
            }
        );
        hold.release.notify_one();
        f.settled().await;
        f.assert_records(&["attempt", "withdrawn"]);
        assert_eq!(f.prompts.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn deadline_answers_during_held_append_then_records_timeout() {
    for privileged in [false, true] {
        let hold = gate();
        let f = ShortcutFixture::new(
            Hooks {
                before_append: Some(hold.clone()),
                ..Default::default()
            },
            WriterHooks::default(),
        );
        let mut running = f.start(privileged).await;
        entered(&hold).await;
        tokio::time::pause();
        tokio::time::advance(OperatorLimits::default().delivery_budget()).await;
        tokio::time::resume();
        assert_eq!(running.answer().await["detail"]["reason"], "timeout");
        hold.release.notify_one();
        f.settled().await;
        f.assert_records(&["attempt", "timeout"]);
        assert_eq!(f.prompts.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn append_errors_handoff_without_grant_and_outcome_failure_cannot_change_answer() {
    for privileged in [false, true] {
        for error in ["substituted", "rejected", "sink-closing", "outcome"] {
            let f = ShortcutFixture::new(
                Hooks {
                    substitute_error: error == "substituted",
                    reject_attempt: error == "rejected",
                    fail_outcome: error == "outcome",
                    ..Default::default()
                },
                WriterHooks {
                    write_failure: error == "sink-closing",
                    ..Default::default()
                },
            );
            let mut running = f.start(privileged).await;
            let answer = running.answer().await;
            f.settled().await;
            if error == "outcome" {
                assert_eq!(answer["code"], "confirmed");
                f.assert_records(&["attempt"]);
                assert_eq!(f.prompts.load(Ordering::SeqCst), 0);
            } else {
                assert_eq!(answer["detail"]["reason"], "person");
                f.assert_records(match error {
                    "substituted" => &["attempt", "append_failed"],
                    "rejected" => &["append_failed"],
                    _ => &[],
                });
                assert_eq!(f.prompts.load(Ordering::SeqCst), 1);
            }
        }
    }
}

#[tokio::test]
async fn append_error_then_cancel_at_handoff_never_prompts() {
    for privileged in [false, true] {
        let hold = gate();
        let f = ShortcutFixture::new(
            Hooks {
                substitute_error: true,
                before_handoff: Some(hold.clone()),
                ..Default::default()
            },
            WriterHooks::default(),
        );
        let mut running = f.start(privileged).await;
        entered(&hold).await;
        assert!(f
            .f
            .forwarding
            .operator_confirms()
            .cancel(running.module.ctx.connection_id, 91));
        assert_eq!(
            running.answer().await["detail"]["reason"],
            "caller_cancelled"
        );
        hold.release.notify_one();
        f.settled().await;
        f.assert_records(&["attempt", "append_failed"]);
        assert_eq!(f.prompts.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn absent_ledger_prompts_without_records() {
    for privileged in [false, true] {
        let mut f = ShortcutFixture::new(Hooks::default(), WriterHooks::default());
        f.context.ledger = None;
        let mut running = f.start(privileged).await;
        assert_eq!(running.answer().await["detail"]["reason"], "person");
        f.settled().await;
        f.assert_records(&[]);
        assert_eq!(f.prompts.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn rate_limited_attempt_never_grants() {
    for privileged in [false, true] {
        let f = ShortcutFixture::new(
            Hooks::default(),
            WriterHooks {
                clock: Some(Arc::new(|| 0)),
                ..Default::default()
            },
        );
        let ledger = f.context.ledger.as_ref().unwrap();
        for _ in 0..60 {
            ledger
                .record(
                    "subc".into(),
                    subc_control::ActivityRecordRequest {
                        kind: "subc.filler".into(),
                        summary: "Fill rate bucket".into(),
                        subject: None,
                        details: None,
                        on: None,
                    },
                    subc_control::ActivityActor {
                        principal: Principal::Direct,
                        scope_owner: None,
                        agent_id: None,
                        run_id: None,
                        flow_id: None,
                        project_root: None,
                        workdir: None,
                        project_id: None,
                    },
                    0,
                )
                .await
                .unwrap();
        }
        let mut running = f.start(privileged).await;
        assert_eq!(running.answer().await["detail"]["reason"], "person");
        f.settled().await;
        assert_eq!(f.records().len(), 60);
        assert!(f
            .records()
            .iter()
            .all(|record| record["kind"] == "subc.filler"));
        assert_eq!(f.prompts.load(Ordering::SeqCst), 1);
    }
}

fn publish_self(table: &LaunchTable) -> u64 {
    table.publish(
        "app",
        std::process::id(),
        subc_os::Process::open(std::process::id()).unwrap().unwrap(),
        None,
    )
}
fn launch_stamp(incarnation: u64) -> OperatorAppStamp {
    let mut stamp = OperatorAppStamp::fixture("app", std::process::id());
    stamp.launch_incarnation = Some(incarnation);
    stamp
}

#[tokio::test]
async fn retired_or_replaced_launch_after_revalidation_hands_off_instead_of_granting() {
    for privileged in [false, true] {
        for replace in [false, true] {
            for refuse in [false, true] {
                let hold = gate();
                let mut f = ShortcutFixture::new(
                    Hooks {
                        after_revalidate: Some(hold.clone()),
                        handoff_refused: refuse,
                        ..Default::default()
                    },
                    WriterHooks::default(),
                );
                let incarnation = publish_self(&f.context.launches);
                f.context.stamp = Some(launch_stamp(incarnation));
                f.context.phone_fingerprint = None;
                let mut running = f.start(privileged).await;
                entered(&hold).await;
                f.context.launches.retire("app");
                if replace {
                    let replacement = publish_self(&f.context.launches);
                    assert_ne!(replacement, incarnation);
                    assert!(launch_stamp(replacement).revalidate_with_launches(&f.context.launches));
                }
                hold.release.notify_one();
                let answer = running.answer().await;
                assert_eq!(
                    answer["detail"]["reason"],
                    if refuse { "queue_full" } else { "person" }
                );
                f.settled().await;
                let records = f.records();
                assert_eq!(records.len(), 2, "{records:?}");
                assert_eq!(records[0]["details"]["app"], "app");
                assert_eq!(
                    records[0]["details"]["attempt"],
                    records[1]["details"]["attempt"]
                );
                assert_eq!(
                    records[1]["details"]["outcome"],
                    if refuse {
                        "handoff_refused"
                    } else {
                        "prompted"
                    }
                );
                assert_eq!(f.prompts.load(Ordering::SeqCst), usize::from(!refuse));
            }
        }
    }
}

#[tokio::test]
async fn cancellation_or_deadline_after_append_before_commit_wins() {
    for privileged in [false, true] {
        for cancel in [false, true] {
            let hold = gate();
            let f = ShortcutFixture::new(
                Hooks {
                    after_revalidate: Some(hold.clone()),
                    ..Default::default()
                },
                WriterHooks::default(),
            );
            let mut running = f.start(privileged).await;
            entered(&hold).await;
            if cancel {
                f.f.forwarding
                    .operator_confirms()
                    .cancel(running.module.ctx.connection_id, 91);
            } else {
                tokio::time::pause();
                tokio::time::advance(OperatorLimits::default().delivery_budget()).await;
                tokio::time::resume();
            }
            assert_eq!(
                running.answer().await["detail"]["reason"],
                if cancel {
                    "caller_cancelled"
                } else {
                    "timeout"
                }
            );
            hold.release.notify_one();
            f.settled().await;
            f.assert_records(&["attempt", if cancel { "withdrawn" } else { "timeout" }]);
            assert_eq!(f.prompts.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn near_deadline_handoff_preserves_identity_arrival_and_deadline() {
    for privileged in [false, true] {
        for append_error in [false, true] {
            let hold = gate();
            let mut f = ShortcutFixture::new(
                Hooks {
                    after_append: Some(hold.clone()),
                    substitute_error: append_error,
                    ..Default::default()
                },
                WriterHooks::default(),
            );
            if !append_error {
                f.context.stamp = Some(OperatorAppStamp::fixture("app", 1));
            }
            let (provider, mut events) =
                Controlled::new(ProviderResult::Approved, false, false, Duration::ZERO);
            f.f.forwarding
                .operator_confirms()
                .configure(Arc::new(provider.clone()), OperatorLimits::default());
            let mut running = f.start(privileged).await;
            entered(&hold).await;
            let confirms = f.f.forwarding.operator_confirms();
            let original = {
                let state = confirms.lock();
                let request = &state.shortcuts.values().next().unwrap().request;
                (
                    request.id,
                    request.frame.header.corr,
                    request.arrived,
                    request.delivery_deadline,
                )
            };
            tokio::time::pause();
            tokio::time::advance(
                OperatorLimits::default().delivery_budget() - Duration::from_secs(1),
            )
            .await;
            tokio::time::resume();
            hold.release.notify_one();
            f.settled().await;
            assert!(matches!(event(&mut events).await, Event::Shown(_)));
            {
                let state = confirms.lock();
                let request = &state.prompt.as_ref().unwrap().request;
                assert_eq!(
                    (
                        request.id,
                        request.frame.header.corr,
                        request.arrived,
                        request.delivery_deadline
                    ),
                    original
                );
            }
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::time::resume();
            assert_eq!(running.answer().await["detail"]["reason"], "timeout");
            assert!(
                matches!(event(&mut events).await, Event::Withdraw(reason) if reason == "timeout")
            );
            provider.release();
        }
    }
}

#[tokio::test]
async fn route_close_or_epoch_change_without_notice_is_refused_at_commit() {
    for stale in [false, true] {
        let hold = gate();
        let f = ShortcutFixture::new(
            Hooks {
                after_revalidate: Some(hold.clone()),
                ..Default::default()
            },
            WriterHooks::default(),
        );
        let mut running = f.start(false).await;
        entered(&hold).await;
        f.f.forwarding
            .invalidate_operator_route_for_test(running.module.key, stale);
        hold.release.notify_one();
        assert_eq!(
            running.answer().await["code"],
            error_codes::OPERATOR_REQUEST_NOT_PERMITTED
        );
        f.settled().await;
        f.assert_records(&["attempt", if stale { "stale" } else { "closed" }]);
        assert_eq!(f.prompts.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn revalidation_handoff_rechecks_withdrawal_before_prompting() {
    for privileged in [false, true] {
        let hold = gate();
        let mut f = ShortcutFixture::new(
            Hooks {
                before_handoff: Some(hold.clone()),
                ..Default::default()
            },
            WriterHooks::default(),
        );
        f.context.stamp = Some(OperatorAppStamp::fixture("app", 1));
        let mut running = f.start(privileged).await;
        entered(&hold).await;
        f.f.forwarding
            .operator_confirms()
            .cancel(running.module.ctx.connection_id, 91);
        assert_eq!(
            running.answer().await["detail"]["reason"],
            "caller_cancelled"
        );
        hold.release.notify_one();
        f.settled().await;
        f.assert_records(&["attempt", "withdrawn"]);
        assert_eq!(f.prompts.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn confirm_shortcut_requires_operation_kind_declaration_and_exact_phone_identity() {
    struct Case {
        kind: Option<&'static str>,
        operation: Option<&'static str>,
        declared: Option<Vec<&'static str>>,
        harness: String,
        build: Option<&'static str>,
        principal: Principal,
        phone: bool,
        desktop: bool,
        shortcut: bool,
    }
    let valid = || Case {
        kind: Some("operation"),
        operation: Some("reset"),
        declared: Some(vec!["reset"]),
        harness: "fed-phone:0123456789abcdef0123456789abcdef".into(),
        build: Some("production"),
        principal: Principal::Reserved {
            module_id: "callosum".into(),
        },
        phone: true,
        desktop: false,
        shortcut: true,
    };
    let mut cases = vec![valid()];
    for kind in [None, Some("enrollment"), Some("unknown")] {
        cases.push(Case {
            kind,
            shortcut: false,
            ..valid()
        });
    }
    for operation in [None, Some("other")] {
        cases.push(Case {
            operation,
            shortcut: false,
            ..valid()
        });
    }
    for declared in [
        None,
        Some(vec![]),
        Some(vec!["missing"]),
        Some(vec!["reset", "reset"]),
    ] {
        cases.push(Case {
            declared,
            shortcut: false,
            ..valid()
        });
    }
    for build in [None, Some("development"), Some("Production"), Some("")] {
        cases.push(Case {
            build,
            shortcut: false,
            ..valid()
        });
    }
    for harness in [
        "phone:0123456789abcdef0123456789abcdef",
        "fed-phone:0123456789abcdef0123456789abcde",
        "fed-phone:0123456789abcdef0123456789abcdef0",
        "fed-phone:0123456789abcdef0123456789abcdeF",
        "fed-phone:0123456789abcdef0123456789abcdeg",
        "other",
        "fed-phone:",
    ] {
        cases.push(Case {
            harness: harness.into(),
            shortcut: false,
            ..valid()
        });
    }
    for principal in [
        Principal::Direct,
        Principal::Reserved {
            module_id: "other".into(),
        },
    ] {
        cases.push(Case {
            principal,
            shortcut: false,
            ..valid()
        });
    }
    cases.push(Case {
        phone: false,
        desktop: true,
        shortcut: false,
        ..valid()
    });
    for (index, case) in cases.into_iter().enumerate() {
        let dir = ScratchDir::new("shortcut-eligibility");
        let prompts = Arc::new(AtomicUsize::new(0));
        let mut f = Fixture::new(
            Arc::new(Prompts(prompts.clone())),
            OperatorLimits::default(),
        );
        f.handler = Arc::new(
            (*f.handler)
                .clone()
                .with_activity_ledger(dir.path(), "eligibility".into())
                .with_test_phone_shortcut(case.phone),
        );
        Arc::get_mut(&mut f.handler)
            .unwrap()
            .operator_app_policy
            .windows_enabled = case.desktop;
        let mut module = f
            .module_with_confirm_operations(
                "target",
                51,
                "nonce",
                case.declared
                    .map(|ops| ops.into_iter().map(String::from).collect()),
            )
            .await;
        f.forwarding
            .alter_operator_binding_for_test(module.key, |binding| {
                binding.principal = case.principal;
                binding.harness = case.harness;
                binding.build_class = case.build.map(String::from);
            });
        let mut request = serde_json::json!({"op":"operator.confirm", "summary":"private summary",
            "route_channel":module.key.channel, "route_epoch":module.epoch});
        if let Some(kind) = case.kind {
            request["kind"] = kind.into();
        }
        if let Some(operation) = case.operation {
            request["operation"] = operation.into();
        }
        assert!(f
            .handler
            .handle_control_frame(&module.ctx, frame(FrameType::Request, 91, request))
            .await
            .unwrap()
            .is_empty());
        let answer = receive(&mut module.rx).await;
        assert_eq!(
            answer.header.ty == FrameType::Response,
            case.shortcut,
            "case {index}: {}",
            body(&answer)
        );
        assert_eq!(
            prompts.load(Ordering::SeqCst),
            usize::from(!case.shortcut),
            "case {index}"
        );
        let records: Vec<serde_json::Value> =
            std::fs::read_to_string(dir.path().join("activity.jsonl"))
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .filter(|r: &serde_json::Value| {
                    r["kind"].as_str().unwrap().starts_with("operator_shortcut")
                })
                .collect();
        if case.shortcut {
            assert!(records.iter().any(|r| r["kind"] == "operator_shortcut"));
        } else {
            assert!(records.is_empty(), "case {index}: {records:?}");
        }
    }
}

#[tokio::test]
async fn desktop_control_shortcuts_and_privileged_enforce_dispatch_are_audit_gated() {
    for privileged in [false, true] {
        for (enabled, stale) in [(false, false), (true, false), (true, true)] {
            let dir = ScratchDir::new("shortcut-desktop-control");
            let prompts = Arc::new(AtomicUsize::new(0));
            let mut f = Fixture::new(
                Arc::new(Prompts(prompts.clone())),
                OperatorLimits::default(),
            );
            let incarnation = publish_self(&f.handler.launch_table);
            f.handler = Arc::new(
                (*f.handler)
                    .clone()
                    .with_activity_ledger(dir.path(), "desktop".into())
                    .with_privileged_operator_prompt(
                        crate::daemon_config::PrivilegedOperatorPrompt::Enforce,
                    ),
            );
            Arc::get_mut(&mut f.handler)
                .unwrap()
                .operator_app_policy
                .windows_enabled = enabled;
            let mut module = f
                .module_with_confirm_operations("target", 51, "nonce", Some(vec!["reset".into()]))
                .await;
            let stamp = launch_stamp(incarnation);
            if stale {
                publish_self(&f.handler.launch_table);
            }
            let eligible = enabled && !stale;
            if privileged {
                let (client, _rx) = ctx_fn(901);
                f.handler
                    .operator_apps
                    .lock()
                    .unwrap()
                    .insert(client.connection_id, stamp);
                let answers = f
                    .handler
                    .handle_control_frame(&client, restart(91))
                    .await
                    .unwrap();
                assert_eq!(answers.len(), 1);
                assert_eq!(
                    body(&answers[0])["code"],
                    if eligible {
                        "unknown_module"
                    } else {
                        error_codes::OPERATOR_DECLINED
                    },
                    "only an approved request reaches the restart handler"
                );
            } else {
                f.forwarding
                    .alter_operator_binding_for_test(module.key, |binding| {
                        binding.operator_app = Some(stamp)
                    });
                let request = serde_json::json!({"op":"operator.confirm", "summary":"private summary",
                    "kind":"operation", "operation":"reset", "route_channel":module.key.channel, "route_epoch":module.epoch});
                assert!(f
                    .handler
                    .handle_control_frame(&module.ctx, frame(FrameType::Request, 91, request))
                    .await
                    .unwrap()
                    .is_empty());
                assert_eq!(
                    receive(&mut module.rx).await.header.ty == FrameType::Response,
                    eligible
                );
            }
            assert_eq!(prompts.load(Ordering::SeqCst), usize::from(!eligible));
            let text =
                std::fs::read_to_string(dir.path().join("activity.jsonl")).unwrap_or_default();
            assert_eq!(text.contains("\"kind\":\"operator_shortcut\""), eligible);
            assert!(!text.contains("private summary"));
        }
    }
}

#[tokio::test]
async fn stamped_privileged_log_mode_stays_unchanged_and_writes_no_shortcut_record() {
    let dir = ScratchDir::new("shortcut-log-mode");
    let prompts = Arc::new(AtomicUsize::new(0));
    let mut f = Fixture::new(
        Arc::new(Prompts(prompts.clone())),
        OperatorLimits::default(),
    );
    let incarnation = publish_self(&f.handler.launch_table);
    f.handler = Arc::new(
        (*f.handler)
            .clone()
            .with_activity_ledger(dir.path(), "log-mode".into()),
    );
    Arc::get_mut(&mut f.handler)
        .unwrap()
        .operator_app_policy
        .windows_enabled = true;
    let (client, _rx) = ctx_fn(901);
    f.handler
        .operator_apps
        .lock()
        .unwrap()
        .insert(client.connection_id, launch_stamp(incarnation));
    let answers = f
        .handler
        .handle_control_frame(&client, restart(91))
        .await
        .unwrap();
    assert_eq!(body(&answers[0])["code"], "unknown_module");
    assert_eq!(prompts.load(Ordering::SeqCst), 0);
    assert!(!std::fs::read_to_string(dir.path().join("activity.jsonl"))
        .unwrap_or_default()
        .contains("operator_shortcut"));
}

struct HeldWriter(Arc<(Mutex<bool>, Condvar)>);
impl HeldWriter {
    fn new() -> Self {
        Self(Arc::new((Mutex::new(true), Condvar::new())))
    }
    fn release(&self) {
        *self.0 .0.lock().unwrap() = false;
        self.0 .1.notify_all();
    }
}
impl Drop for HeldWriter {
    fn drop(&mut self) {
        self.release();
    }
}

#[tokio::test]
async fn withdrawal_and_deadline_do_not_cancel_an_append_already_waiting_for_the_writer() {
    for privileged in [false, true] {
        for cancel in [false, true] {
            let hold = HeldWriter::new();
            let f = ShortcutFixture::new(
                Hooks::default(),
                WriterHooks {
                    pause: Some(hold.0.clone()),
                    ..Default::default()
                },
            );
            let mut running = f.start(privileged).await;
            let ledger = f.context.ledger.as_ref().unwrap();
            // The writer is paused before receiving, so a consumed queue slot
            // proves that record() has submitted the append and awaits its reply.
            tokio::time::timeout(Duration::from_secs(4), async {
                while ledger.remaining_capacity() == 64 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            if cancel {
                f.f.forwarding
                    .operator_confirms()
                    .cancel(running.module.ctx.connection_id, 91);
            } else {
                tokio::time::pause();
                tokio::time::advance(OperatorLimits::default().delivery_budget()).await;
                tokio::time::resume();
            }
            assert_eq!(
                running.answer().await["detail"]["reason"],
                if cancel {
                    "caller_cancelled"
                } else {
                    "timeout"
                }
            );
            assert!(f.records().is_empty());
            hold.release();
            f.settled().await;
            f.assert_records(&["attempt", if cancel { "withdrawn" } else { "timeout" }]);
            assert_eq!(f.prompts.load(Ordering::SeqCst), 0);
        }
    }
}

#[test]
fn shortcut_audit_kind_exemptions_belong_only_to_the_daemon() {
    for kind in [
        "operator_shortcut",
        "operator_shortcut_outcome",
        "operator_shortcut_other",
    ] {
        let request = subc_control::ActivityRecordRequest {
            kind: kind.into(),
            summary: "Fixed audit summary".into(),
            subject: None,
            details: None,
            on: None,
        };
        for module in ["subc", "worker", "subc_other"] {
            assert_eq!(
                crate::activity::validate(module, &request).is_ok(),
                module == "subc" && kind != "operator_shortcut_other"
            );
        }
    }
}

#[cfg(windows)]
struct LaunchedWindowsApp(subc_os::Process);
#[cfg(windows)]
impl LaunchedWindowsApp {
    fn stop(&self) {
        if let Some(observed) = self.0.observe() {
            self.0
                .force_stop(observed.start_time, Duration::from_secs(10))
                .unwrap();
        }
    }
}
#[cfg(windows)]
impl Drop for LaunchedWindowsApp {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(windows)]
#[tokio::test]
async fn windows_app_open_replacement_while_paused_after_revalidation_never_grants() {
    for privileged in [false, true] {
        let hold = gate();
        let mut f = ShortcutFixture::new(
            Hooks {
                after_revalidate: Some(hold.clone()),
                ..Default::default()
            },
            WriterHooks::default(),
        );
        let path = PathBuf::from(std::env::var("SystemRoot").unwrap())
            .join("System32")
            .join("notepad.exe");
        f.f.handler = Arc::new(
            (*f.f.handler)
                .clone()
                .with_test_operator_app_policy(crate::operator_apps::TestOperatorAppPolicy {
                    macos_enabled: false,
                    windows_enabled: true,
                    linux_enabled: false,
                    app: "app",
                    requirement: "always",
                    floor: semver::Version::new(1, 2, 10),
                    windows_publisher: Some("test-publisher"),
                    windows_path: Some(path),
                    linux_path: None,
                    windows_breakaway: false,
                    linux_force_direct: false,
                })
                .with_test_windows_verifier(Arc::new(|_, _| Ok("1.2.10".into()))),
        );
        f.context.launches = f.f.handler.launch_table.clone();
        let (client, _rx) = ctx_fn(902);
        let open = || {
            frame(
                FrameType::Request,
                92,
                serde_json::json!({"op":"app.open","app":"app"}),
            )
        };
        let first =
            f.f.handler
                .handle_control_frame(&client, open())
                .await
                .unwrap();
        assert_eq!(body(&first[0])["status"], "launched");
        let old = f.context.launches.live_entry("app").unwrap();
        let old_process = LaunchedWindowsApp(subc_os::Process::open(old.pid).unwrap().unwrap());
        f.context.stamp = Some(launch_stamp(old.incarnation));
        f.context.stamp.as_mut().unwrap().app.pid = old.pid;
        f.context.phone_fingerprint = None;
        let mut running = f.start(privileged).await;
        entered(&hold).await;
        // The original route remains open while its launch exits. A replacement
        // launched by app.open must not make the original connection's old
        // launch stamp valid again.
        old_process.stop();
        let replacement =
            f.f.handler
                .handle_control_frame(&client, open())
                .await
                .unwrap();
        assert_eq!(body(&replacement[0])["status"], "launched");
        let new = f.context.launches.live_entry("app").unwrap();
        let _replacement_process =
            LaunchedWindowsApp(subc_os::Process::open(new.pid).unwrap().unwrap());
        assert_ne!(old.incarnation, new.incarnation);
        assert!(launch_stamp(new.incarnation).revalidate_with_launches(&f.context.launches));
        hold.release.notify_one();
        assert_eq!(running.answer().await["detail"]["reason"], "person");
        f.settled().await;
        let records = f.records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["details"]["app"], "app");
        assert_eq!(records[1]["details"]["outcome"], "prompted");
        assert_eq!(
            records[0]["details"]["attempt"],
            records[1]["details"]["attempt"]
        );
    }
}
