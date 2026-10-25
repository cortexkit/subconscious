use std::collections::VecDeque;

use ck_ui::live::actions::{busy_count, busy_gauges, run_action, ActionSource, Reading, Watch};
use ck_ui::live::{LiveProcess, State};
use ck_ui::model::ActionKind;
use ck_ui::time::Timestamp;
use futures_util::future::BoxFuture;
use serde_json::json;
use subc_control::{SupervisorEntry, SupervisorHealthStatus as H};

fn reading(at: Timestamp, pid: Option<u32>, status: Option<&str>, busy: Option<u64>) -> Reading {
    let mut state = State::empty("This computer");
    state.roster.set(
        vec![SupervisorEntry::new(
            "broca",
            if pid.is_some() { "running" } else { "stopped" },
            true,
            pid.is_some(),
            H::Unknown,
        )],
        at,
    );
    if let Some(pid) = pid {
        state.processes.insert(
            "broca".into(),
            LiveProcess {
                pid,
                generation: u64::from(pid),
                spawned_at: at,
            },
        );
    }
    if let Some(status) = status {
        state.health.set(vec![serde_json::from_value(json!({"module_id":"broca", "status":status, "consecutive_failures":0, "last_probe_ms":at.unix_millis() + 2100})).unwrap()], at);
    }
    Reading {
        state,
        busy,
        busy_declared: busy.is_some(),
        budget_ms: 30_000,
    }
}

struct Script {
    steps: std::sync::Mutex<VecDeque<Reading>>,
    sent: std::sync::Mutex<Vec<(ActionKind, String, Option<u64>)>>,
}

struct HeldAck(Script);

impl ActionSource for HeldAck {
    type Error = ();
    fn read(&self) -> BoxFuture<'_, Result<Reading, ()>> {
        self.0.read()
    }
    fn send(
        &self,
        kind: ActionKind,
        module: &str,
        drain: Option<u64>,
    ) -> BoxFuture<'_, Result<bool, ()>> {
        self.0
            .sent
            .lock()
            .unwrap()
            .push((kind, module.into(), drain));
        Box::pin(async {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            assert_eq!(
                self.0.steps.lock().unwrap().len(),
                1,
                "the drain must be observed before its acknowledgement"
            );
            Ok(true)
        })
    }
}

impl ActionSource for Script {
    type Error = ();
    fn read(&self) -> BoxFuture<'_, Result<Reading, ()>> {
        Box::pin(async {
            let mut steps = self.steps.lock().unwrap();
            Ok(if steps.len() > 1 {
                steps.pop_front().unwrap()
            } else {
                let last = steps.front().unwrap();
                Reading {
                    state: last.state.clone(),
                    busy: last.busy,
                    busy_declared: last.busy_declared,
                    budget_ms: last.budget_ms,
                }
            })
        })
    }
    fn send(
        &self,
        kind: ActionKind,
        module: &str,
        drain: Option<u64>,
    ) -> BoxFuture<'_, Result<bool, ()>> {
        self.sent.lock().unwrap().push((kind, module.into(), drain));
        Box::pin(async { Ok(true) })
    }
}

fn frame(line: &str) -> String {
    format!("{:<100}", ck_ui::draw::trunc(line, 100))
}

#[tokio::test(start_paused = true)]
async fn a_held_stop_ack_does_not_block_drain_readings() {
    let at = Timestamp::system_now();
    let source = HeldAck(Script {
        steps: std::sync::Mutex::new(VecDeque::from([
            reading(at.add_millis(-60_000), Some(100), None, Some(3)),
            reading(at.add_millis(-60_000), Some(100), None, Some(0)),
            reading(at, None, None, None),
        ])),
        sent: std::sync::Mutex::new(vec![]),
    });
    let result = run_action(&source, ActionKind::Stop, "broca", None, true, |_, _| {})
        .await
        .unwrap();
    assert!(result.ok);
    assert!(result.inline.contains("3 runs finished first"));
}

#[tokio::test(start_paused = true)]
async fn restart_phase_lines_at_100_columns_and_override_reach_the_source() {
    let at = Timestamp::system_now();
    let script = Script {
        steps: std::sync::Mutex::new(VecDeque::from([
            reading(at.add_millis(-60_000), Some(100), None, Some(3)),
            reading(at.add_millis(-60_000), Some(100), None, Some(0)),
            reading(at, None, None, None),
            reading(at.add_millis(100), Some(222), None, Some(0)),
            reading(at.add_millis(100), Some(222), Some("ok"), Some(0)),
        ])),
        sent: std::sync::Mutex::new(vec![]),
    };
    let mut lines = Vec::new();
    let result = run_action(
        &script,
        ActionKind::Restart,
        "broca",
        Some(5100),
        true,
        |_, line| lines.push(frame(&line)),
    )
    .await
    .unwrap();
    assert_eq!(
        *script.sent.lock().unwrap(),
        [(ActionKind::Restart, "broca".into(), Some(5100))]
    );
    assert_eq!(
        lines,
        [
            "broca draining · 3 in flight · budget 5.1 s",
            "broca draining · 0 in flight · budget 5.1 s",
            "broca stopping",
            "broca starting · pid 222",
            "broca waiting for health",
        ]
        .map(|s| format!("{s:<100}"))
    );
    assert!(result.ok);
    assert_eq!(result.plain, "broca restarted · healthy · pid 222");
    assert!(result.new_process);
    assert!(result.inline.contains("3 runs finished first"));
    assert!(result.inline.ends_with("pid 222 · healthy after 2.1 s"));
}

#[tokio::test(start_paused = true)]
async fn start_and_stop_phase_lines_at_100_columns_and_already_stopped() {
    let at = Timestamp::system_now();
    for already_stopped in [false, true] {
        let initial = reading(
            at.add_millis(-1000),
            if already_stopped { None } else { Some(100) },
            None,
            None,
        );
        let script = Script {
            steps: std::sync::Mutex::new(VecDeque::from([initial, reading(at, None, None, None)])),
            sent: std::sync::Mutex::new(vec![]),
        };
        let mut lines = Vec::new();
        let result = run_action(&script, ActionKind::Stop, "broca", None, true, |_, line| {
            lines.push(frame(&line))
        })
        .await
        .unwrap();
        assert!(result.ok);
        assert_eq!(result.plain, "broca stopped");
        assert_eq!(
            lines,
            ["broca draining · budget 30.0 s", "broca stopping"].map(|s| format!("{s:<100}"))
        );
    }
    let script = Script {
        steps: std::sync::Mutex::new(VecDeque::from([
            reading(at, None, None, None),
            reading(at.add_millis(100), Some(222), Some("ok"), None),
        ])),
        sent: std::sync::Mutex::new(vec![]),
    };
    let mut lines = Vec::new();
    let result = run_action(
        &script,
        ActionKind::Start,
        "broca",
        None,
        true,
        |_, line| lines.push(frame(&line)),
    )
    .await
    .unwrap();
    assert!(result.ok);
    assert_eq!(
        lines,
        [
            "broca starting",
            "broca starting · pid 222",
            "broca waiting for health"
        ]
        .map(|s| format!("{s:<100}"))
    );
}

#[test]
fn replacement_zero_does_not_claim_the_old_process_finished_its_runs() {
    let at = Timestamp::from_unix_millis(1_000_000);
    let initial = reading(at.add_millis(-60_000), Some(100), None, Some(3));
    let mut watch = Watch::new(ActionKind::Restart, "broca", &initial, at, None);
    watch.progress(&initial, at.add_millis(30_000), true);
    let replacement = reading(at.add_millis(30_100), Some(222), Some("ok"), Some(0));
    let (_, _, result) = watch.progress(&replacement, at.add_millis(32_200), true);
    let result = result.unwrap();
    assert!(result.ok);
    assert!(
        !result.inline.contains("runs finished first"),
        "{}",
        result.inline
    );
}

#[test]
fn a_reused_pid_is_a_healthy_replacement_when_its_spawn_identity_changes() {
    let at = Timestamp::from_unix_millis(1_000_000);
    let initial = reading(at.add_millis(-60_000), Some(100), None, Some(3));
    for new_generation in [false, true] {
        let mut watch = Watch::new(ActionKind::Restart, "broca", &initial, at, None);
        let mut replacement = reading(at.add_millis(500), Some(100), Some("ok"), Some(0));
        if new_generation {
            replacement
                .state
                .processes
                .get_mut("broca")
                .unwrap()
                .generation += 1;
        }
        let (_, _, result) = watch.progress(&replacement, at.add_millis(2_600), true);
        let result = result.unwrap();
        assert!(result.ok);
        assert!(result.new_process);
        assert_eq!(result.plain, "broca restarted · healthy · pid 100");
        assert!(
            !result.inline.contains("runs finished first"),
            "{}",
            result.inline
        );
    }
}

#[test]
fn start_already_running_reports_the_existing_process_without_claiming_a_spawn() {
    let at = Timestamp::from_unix_millis(1_000_000);
    for seen_before in [true, false] {
        for status in ["ok", "degraded"] {
            let initial = reading(at.add_millis(-60_000), Some(100), Some(status), None);
            let absent = reading(at, None, None, None);
            let mut watch = Watch::new(
                ActionKind::Start,
                "broca",
                if seen_before { &initial } else { &absent },
                at,
                None,
            );
            let (_, _, result) = watch.progress(&initial, at, true);
            let result = result.unwrap();
            assert_eq!(result.ok, status == "ok");
            assert!(!result.new_process);
            assert_eq!(
                result.state,
                if status == "ok" {
                    "healthy"
                } else {
                    "degraded"
                }
            );
            assert!(result.plain.starts_with("broca already running · "));
            assert!(result.plain.ends_with("pid 100"));
            assert!(!result.inline.contains("started"), "{}", result.inline);
            assert!(!result.detail.contains("started"), "{}", result.detail);
        }
    }
}

#[test]
fn busy_counts_require_all_declared_gauges_and_never_infer_work() {
    let declarations = serde_json::from_value(json!([
        {"name":"work", "kind":"busy", "effect":"observe", "anchored_to":{"health_gauges":{"gauges":["executing", "queued"]}}},
        {"name":"heartbeat", "kind":"keepalive", "effect":"observe", "anchored_to":{"health_gauges":{"gauges":["in_flight"]}}}
    ])).unwrap();
    let catalog = [
        subc_control::CatalogEntry::new("broca", vec![], vec![])
            .with_self_signals(Some(declarations)),
        subc_control::CatalogEntry::new("other", vec![], vec![]),
    ];
    assert_eq!(busy_gauges(&catalog, "broca"), ["executing", "queued"]);
    assert!(busy_gauges(&catalog, "other").is_empty());
    assert_eq!(
        busy_count(
            &["executing".into(), "queued".into()],
            Some(&json!({"executing":2, "queued":3, "in_flight":99}))
        ),
        Some(5)
    );
    assert_eq!(
        busy_count(&["missing".into()], Some(&json!({"in_flight":3}))),
        None
    );
    assert_eq!(busy_count(&[], Some(&json!({"in_flight":3}))), None);
    let at = Timestamp::from_unix_millis(1_000_000);
    let mut missing = reading(at, Some(100), None, None);
    missing.busy_declared = true;
    assert_eq!(
        Watch::new(ActionKind::Stop, "broca", &missing, at, None).initial_line(),
        "broca draining · unknown in flight · budget 30.0 s"
    );
}

#[tokio::test(start_paused = true)]
async fn unhealthy_results_and_watch_deadlines_are_failures() {
    let at = Timestamp::system_now();
    for status in ["degraded", "failing", "unresponsive"] {
        let mut unhealthy = reading(at.add_millis(100), Some(222), Some(status), None);
        // Producer prose must not override the structured health status.
        unhealthy.state.health.value.as_mut().unwrap()[0].detail =
            Some("degraded before; health unknown".into());
        let script = Script {
            steps: std::sync::Mutex::new(VecDeque::from([
                reading(at.add_millis(-60_000), Some(100), None, None),
                unhealthy,
            ])),
            sent: std::sync::Mutex::new(vec![]),
        };
        let result = run_action(&script, ActionKind::Restart, "broca", None, true, |_, _| {})
            .await
            .unwrap();
        assert!(!result.ok);
        assert_eq!(result.state, status);
        assert!(result.inline.starts_with("✗ "));
    }
    for kind in [ActionKind::Restart, ActionKind::Stop, ActionKind::Start] {
        let script = Script {
            steps: std::sync::Mutex::new(VecDeque::from([reading(
                at.add_millis(-60_000),
                Some(100),
                None,
                None,
            )])),
            sent: std::sync::Mutex::new(vec![]),
        };
        let result = run_action(&script, kind, "broca", None, true, |_, _| {})
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.inline.starts_with("✗ "));
        assert!(result.inline.contains("watch deadline passed"));
        assert!(result.plain.contains("running"));
    }
}
