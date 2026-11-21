use std::{path::PathBuf, sync::Arc, time::Duration};

use subc_protocol::{
    manifest::Concurrency,
    session::{HealthStatus, ModuleControlResponse},
};
use tokio::sync::mpsc;

use super::{
    probe_module_health, HealthAction, HealthConfig, HealthProbeEvidence, ModuleProtocol,
    ModuleSpec, RestartPolicy, Supervisor, SupervisorRuntimeConfig,
};
use crate::{
    control::ControlHandler,
    forwarding::{ForwardingTable, ModuleControlRpcCompletion, ModuleControlRpcOutcome},
    registry::{ConnectionId, Registry},
    router::FrameSink,
};

struct ProbeHarness {
    spec: ModuleSpec,
    runtime: SupervisorRuntimeConfig,
    forwarding: Arc<ForwardingTable>,
    module_connection: ConnectionId,
    module_rx: mpsc::Receiver<crate::router::OutboundFrame>,
    handler: ControlHandler,
    module: super::SupervisedModule,
}

fn probe_harness() -> ProbeHarness {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let supervisor_handle = super::SupervisorHandle::new();
    let health = HealthConfig {
        http: None,
        cadence: Duration::from_secs(30),
        deadline: Duration::from_secs(5),
        failure_threshold: 3,
        on_degraded: HealthAction::Report,
        on_failing: HealthAction::Report,
        critical: false,
    };
    let supervisor = Supervisor::new_for_test(Arc::clone(&registry), RestartPolicy::default())
        .with_forwarding(Arc::clone(&forwarding))
        .with_handle(supervisor_handle.clone())
        .with_health_config(health);
    let spec = ModuleSpec {
        script: None,
        module_id: "late-health-module".to_string(),
        program: PathBuf::from("disabled-module"),
        args: Vec::new(),
        env: Vec::new(),
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    };
    let module = supervisor
        .supervise_configured(spec.clone(), false)
        .unwrap();
    let runtime = supervisor.runtime_config();
    let handler = ControlHandler::with_forwarding(registry, Arc::clone(&forwarding))
        .with_supervisor(supervisor_handle);
    let module_connection = ConnectionId::new(700);
    let (module_tx, module_rx) = mpsc::channel(8);
    forwarding
        .register_module_connection(
            module_connection,
            spec.module_id.clone(),
            subc_protocol::PROTOCOL_VERSION,
            Concurrency::ModuleManaged,
            FrameSink::new(module_tx),
        )
        .unwrap();

    ProbeHarness {
        spec,
        runtime,
        forwarding,
        module_connection,
        module_rx,
        handler,
        module,
    }
}

async fn finish_after(harness: &mut ProbeHarness, stall: Duration) -> ModuleControlRpcCompletion {
    assert!(stall > harness.runtime.health.deadline);
    let deadline = harness.runtime.health.deadline;
    let probe = probe_module_health(&harness.spec.module_id, &harness.runtime, None);
    let answer = async {
        let frame = harness.module_rx.recv().await.expect("health.check frame");
        tokio::time::advance(deadline).await;
        tokio::task::yield_now().await;
        tokio::time::advance(stall - deadline).await;
        harness
            .forwarding
            .complete_module_control_rpc(
                harness.module_connection,
                frame.header.corr,
                Some("health.check"),
                ModuleControlRpcOutcome::Response(ModuleControlResponse::HealthCheck {
                    status: HealthStatus::Ok,
                    detail: None,
                    metrics: None,
                }),
            )
            .unwrap()
    };
    let (probe_result, completion) = tokio::join!(probe, answer);
    let err = probe_result.expect_err("probe must miss its deadline");
    assert!(matches!(err.evidence, HealthProbeEvidence::NoAnswer));
    completion
}

async fn time_out_without_answer(harness: &mut ProbeHarness) {
    let deadline = harness.runtime.health.deadline;
    let probe = probe_module_health(&harness.spec.module_id, &harness.runtime, None);
    let exhaust_deadline = async {
        let _frame = harness.module_rx.recv().await.expect("health.check frame");
        tokio::time::advance(deadline).await;
        tokio::task::yield_now().await;
    };
    let (probe_result, ()) = tokio::join!(probe, exhaust_deadline);
    let err = probe_result.expect_err("probe must miss its deadline");
    assert!(matches!(err.evidence, HealthProbeEvidence::NoAnswer));
}

async fn run_probe_cycle(harness: &mut ProbeHarness, answer: bool) {
    let registry = Arc::clone(&harness.module.inner.registry);
    let snapshot = Arc::clone(&harness.module.inner.snapshot);
    let process_liveness = super::SupervisorProcessLiveness::default();
    let mut child = None;
    let cycle = super::run_health_probe_cycle(
        &harness.spec,
        &harness.runtime,
        &registry,
        &process_liveness,
        &snapshot,
        &mut child,
    );
    let peer = async {
        let frame = harness.module_rx.recv().await.expect("health.check frame");
        if answer {
            harness
                .forwarding
                .complete_module_control_rpc(
                    harness.module_connection,
                    frame.header.corr,
                    Some("health.check"),
                    ModuleControlRpcOutcome::Response(ModuleControlResponse::HealthCheck {
                        status: HealthStatus::Ok,
                        detail: None,
                        metrics: Some(serde_json::json!({"ready": true})),
                    }),
                )
                .unwrap();
        } else {
            tokio::time::advance(harness.runtime.health.deadline).await;
            tokio::task::yield_now().await;
        }
    };
    tokio::join!(cycle, peer);
}

#[tokio::test(start_paused = true)]
async fn unanswered_probe_is_unknown_until_threshold_and_ok_report_recovers() {
    let mut harness = probe_harness();
    // Drive the probe cycle directly with an in-memory wire peer. Stop the
    // disabled module's monitor so only this test owns lifecycle transitions;
    // no OS process is launched, and a restart is observed at scheduling.
    let monitor = harness.module.inner.monitor.lock().unwrap().take().unwrap();
    monitor.abort();
    let _ = monitor.await;
    super::update_snapshot(&harness.module.inner.snapshot, None, |state| {
        state.enabled = true;
        state.state = super::ModuleState::Running;
        state.process_alive = true;
    })
    .unwrap();

    run_probe_cycle(&mut harness, true).await;
    assert_eq!(
        harness.module.status().unwrap().health.status,
        super::SupervisorHealthStatus::Ok
    );

    for failures in 1..harness.runtime.health.failure_threshold {
        run_probe_cycle(&mut harness, false).await;
        let status = harness.module.status().unwrap();
        assert_eq!(status.health.status, super::SupervisorHealthStatus::Unknown);
        assert_eq!(status.health.consecutive_failures, failures);
        assert!(status.health.last_probe_ms.is_some());
        assert!(status.health.detail.unwrap().starts_with("[no-answer]"));
        assert!(status.health.metrics.is_none());
        assert_eq!(status.state, super::ModuleState::Running);
        assert!(status.process_alive);
        assert_eq!(status.restart_count, 0);
        assert_eq!(status.lifetime_restarts, 0);
        assert!(status.health.last_action.is_none());
        assert!(harness.runtime.scheduled_respawn.lock().unwrap().is_none());
    }

    run_probe_cycle(&mut harness, true).await;
    let recovered = harness.module.status().unwrap();
    assert_eq!(recovered.health.status, super::SupervisorHealthStatus::Ok);
    assert_eq!(recovered.health.consecutive_failures, 0);
    assert!(recovered.health.detail.is_none());
    assert_eq!(
        recovered.health.metrics,
        Some(serde_json::json!({"ready": true}))
    );
    assert_eq!(recovered.lifetime_restarts, 0);

    for failures in 1..=harness.runtime.health.failure_threshold {
        run_probe_cycle(&mut harness, false).await;
        let status = harness.module.status().unwrap();
        assert_eq!(status.health.consecutive_failures, failures);
        if failures < harness.runtime.health.failure_threshold {
            assert_eq!(status.health.status, super::SupervisorHealthStatus::Unknown);
            assert_eq!(status.state, super::ModuleState::Running);
            assert_eq!(status.lifetime_restarts, 0);
            assert!(harness.runtime.scheduled_respawn.lock().unwrap().is_none());
        } else {
            assert_eq!(
                status.health.status,
                super::SupervisorHealthStatus::Unresponsive
            );
            assert_eq!(status.state, super::ModuleState::Restarting);
            // Count the replacement at spawn admission; this harness only schedules it.
            assert_eq!((status.restart_count, status.lifetime_restarts), (0, 0));
            assert_eq!(status.health.last_action.as_deref(), Some("restart"));
            assert!(harness.runtime.scheduled_respawn.lock().unwrap().is_some());
        }
    }
}

#[tokio::test(start_paused = true)]
async fn late_health_answers_record_start_anchored_latency_for_two_stalls() {
    let mut harness = probe_harness();

    let first = finish_after(&mut harness, Duration::from_secs(8)).await;
    let first_latency = match &first {
        ModuleControlRpcCompletion::LateHealthAnswer { latency, .. } => *latency,
        other => panic!("late answer was not retained: {other:?}"),
    };
    assert!(harness.handler.observe_module_control_completion(first));

    let second = finish_after(&mut harness, Duration::from_secs(11)).await;
    let second_latency = match &second {
        ModuleControlRpcCompletion::LateHealthAnswer { latency, .. } => *latency,
        other => panic!("late answer was not retained: {other:?}"),
    };
    assert!(harness.handler.observe_module_control_completion(second));

    assert_eq!(first_latency, Duration::from_secs(8));
    assert_eq!(
        second_latency - first_latency,
        Duration::from_secs(3),
        "latency must grow linearly with the additional stall"
    );
    let health = harness.module.status().unwrap().health;
    assert_eq!(health.late_answer_count, 2);
    assert_eq!(health.last_late_answer_latency_ms, Some(11_000));
}

/// A module that answers every probe late must never march to the kill
/// threshold: the late answer proves it is alive, so it must clear the miss
/// streak the timeout recorded. Without the reset, a CPU-starved module
/// that serves every probe seconds past the deadline accumulates
/// `consecutive_failures` to the threshold and is killed — the exact
/// sequence from the 2026-08-14 aft disable, where the daemon logged
/// "proves the module is alive" five times while counting five misses.
#[tokio::test(start_paused = true)]
async fn late_answer_clears_the_consecutive_failure_streak() {
    let mut harness = probe_harness();

    // Timeout recorded first: the probe path saw no answer in time.
    time_out_without_answer(&mut harness).await;
    harness
        .module
        .record_health_probe_failure_for_test("[no-answer] test miss")
        .unwrap();
    assert_eq!(
        harness.module.status().unwrap().health.consecutive_failures,
        1,
        "precondition: the miss must be on the streak before the late answer"
    );

    // The stalled reply then lands: proof of life.
    let late = finish_after(&mut harness, Duration::from_secs(9)).await;
    assert!(matches!(
        late,
        ModuleControlRpcCompletion::LateHealthAnswer { .. }
    ));
    assert!(harness.handler.observe_module_control_completion(late));

    let health = harness.module.status().unwrap().health;
    assert_eq!(
        health.consecutive_failures, 0,
        "a late answer is an answer: the streak must reset"
    );
    assert_eq!(health.late_answer_count, 1);
}

#[tokio::test(start_paused = true)]
async fn repeated_serial_probe_cycles_keep_one_tombstone_per_endpoint() {
    let mut harness = probe_harness();

    for _ in 0..20 {
        time_out_without_answer(&mut harness).await;
        assert_eq!(
            harness.forwarding.health_probe_tombstone_count().unwrap(),
            1
        );
    }
}
