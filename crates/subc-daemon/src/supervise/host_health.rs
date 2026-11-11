use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use tokio::{
    task::JoinHandle,
    time::{interval_at, Instant, MissedTickBehavior},
};
use tracing::info;

use super::{
    HealthProbeEvidence, ModuleProtocol, ModuleSpec, ModuleState, SharedSnapshot,
    SupervisorHealthStatus, SupervisorSnapshot,
};

const WINDOW: Duration = Duration::from_secs(60);
const TICK: Duration = Duration::from_secs(1);
const LAG_LIMIT: Duration = Duration::from_secs(2);

/// One signal for the daemon, shared by all module actors and late-reply observers.
/// Weak snapshots let the roster change without retaining retired module handles.
#[derive(Debug, Clone, Default)]
pub(super) struct HostHealth(Arc<Mutex<State>>);

#[derive(Debug)]
struct TrackedModule {
    snapshot: Weak<Mutex<SupervisorSnapshot>>,
    configured_protocol: ModuleProtocol,
}

#[derive(Debug, Default)]
struct State {
    modules: HashMap<String, TrackedModule>,
    late_modules: HashMap<String, Instant>,
    lag_exceeded: Option<(Instant, Duration)>,
    latest_tick_lag: Duration,
    held: bool,
}

impl HostHealth {
    pub(super) fn track(&self, spec: &ModuleSpec, snapshot: &SharedSnapshot) {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .modules
            .insert(
                spec.module_id.clone(),
                TrackedModule {
                    snapshot: Arc::downgrade(snapshot),
                    configured_protocol: spec.protocol,
                },
            );
    }

    pub(super) fn record_late_module(&self, module_id: &str) -> bool {
        let now = Instant::now();
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        state.late_modules.insert(module_id.to_owned(), now);
        state.refresh(now)
    }

    pub(super) fn spawn_monitor(&self) -> JoinHandle<()> {
        let monitor = self.clone();
        // Set the first deadline before spawning: delay in scheduling this task
        // is itself daemon lag, including on the first tick.
        let mut ticks = interval_at(Instant::now() + TICK, TICK);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        tokio::spawn(async move {
            loop {
                let expected = ticks.tick().await;
                monitor.record_tick(expected, Instant::now());
            }
        })
    }

    fn record_tick(&self, expected: Instant, now: Instant) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let lag = now.saturating_duration_since(expected);
        state.latest_tick_lag = lag;
        if lag > LAG_LIMIT {
            // On Windows, a tick can read late after host sleep. Holding
            // silence-based restarts for one window after wake is harmless.
            state.lag_exceeded = Some((now, lag));
        }
        state.refresh(now);
    }
}

impl State {
    fn refresh(&mut self, now: Instant) -> bool {
        self.late_modules
            .retain(|_, at| now.saturating_duration_since(*at) < WINDOW);
        self.lag_exceeded = self
            .lag_exceeded
            .filter(|(at, _)| now.saturating_duration_since(*at) < WINDOW);
        self.modules
            .retain(|_, module| module.snapshot.strong_count() > 0);
        let running_subc = self
            .modules
            .values()
            .filter_map(|module| {
                let snapshot = module.snapshot.upgrade()?;
                let state = snapshot.lock().unwrap_or_else(|p| p.into_inner());
                Some(
                    state.process_alive
                        && state.spawned_protocol.unwrap_or(module.configured_protocol)
                            == ModuleProtocol::Subc,
                )
            })
            .filter(|running| *running)
            .count();
        let threshold = 3.max(running_subc.div_ceil(4));
        let held = self.late_modules.len() >= threshold || self.lag_exceeded.is_some();
        if held != self.held {
            info!(
                late_modules = self.late_modules.len(),
                running_subc,
                threshold,
                daemon_lag_ms = self
                    .lag_exceeded
                    .map_or(self.latest_tick_lag, |(_, lag)| lag)
                    .as_millis() as u64,
                "{}",
                if held {
                    "host overloaded; holding no-answer health restarts"
                } else {
                    "host overload cleared; resuming no-answer health restarts"
                }
            );
            self.held = held;
        }
        if held {
            // The first failures can arrive before the shared signal crosses
            // its threshold. Include those modules in the hold too, but never
            // overwrite a newer report or evidence of a dead control lane.
            for module in self.modules.values() {
                let Some(snapshot) = module.snapshot.upgrade() else {
                    continue;
                };
                let mut state = snapshot.lock().unwrap_or_else(|p| p.into_inner());
                if state.state == ModuleState::Running
                    && state.last_probe_evidence == Some(HealthProbeEvidence::NoAnswer)
                {
                    state.health.status = SupervisorHealthStatus::Degraded;
                    state.health.consecutive_failures = 0;
                    let detail = state.health.detail.get_or_insert_with(String::new);
                    if !detail.contains("host overloaded") {
                        detail.push_str("; host overloaded");
                    }
                }
            }
        }
        held
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::supervise::{
        handle_health_probe_failure, HealthAction, HealthConfig, HealthProbeError, RestartPolicy,
        SupervisedModule, Supervisor, SupervisorHandle, SupervisorRuntimeConfig,
    };

    #[derive(Clone, Default)]
    struct Events(Arc<Mutex<Vec<HashMap<String, String>>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Events {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() != tracing::Level::INFO {
                return;
            }
            struct Fields(HashMap<String, String>);
            impl tracing::field::Visit for Fields {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0.insert(field.name().to_owned(), format!("{value:?}"));
                }
            }
            let mut fields = Fields(HashMap::new());
            event.record(&mut fields);
            self.0.lock().unwrap().push(fields.0);
        }
    }

    struct Module {
        spec: ModuleSpec,
        runtime: SupervisorRuntimeConfig,
        handle: SupervisedModule,
    }

    struct Fleet {
        supervisor: Supervisor,
        handle: SupervisorHandle,
        modules: Vec<Module>,
    }

    impl Fleet {
        fn new(count: usize) -> Self {
            let handle = SupervisorHandle::new();
            let supervisor = Supervisor::new(Default::default(), RestartPolicy::default())
                .with_handle(handle.clone())
                .with_health_config(HealthConfig {
                    // The hold must not escalate even when degraded reports
                    // ordinarily request a restart.
                    on_degraded: HealthAction::Restart,
                    ..HealthConfig::default()
                });
            let modules = (0..count)
                .map(|index| {
                    let spec = ModuleSpec {
                        module_id: format!("health-peer-{index}"),
                        program: PathBuf::from("unused-in-memory-peer"),
                        args: Vec::new(),
                        env: Vec::new(),
                        reserved: false,
                        reserved_prefixes: Vec::new(),
                        protocol: ModuleProtocol::Subc,
                        overlap: Default::default(),
                        carry_containment: false,
                    };
                    let runtime = supervisor.runtime_config();
                    let mut state = SupervisorSnapshot::new(ModuleState::Running, true);
                    state.process_alive = true;
                    state.spawned_protocol = Some(ModuleProtocol::Subc);
                    let (handle, _) =
                        supervisor.register_module(&spec, &runtime, Arc::new(Mutex::new(state)));
                    Module {
                        spec,
                        runtime,
                        handle,
                    }
                })
                .collect();
            Self {
                supervisor,
                handle,
                modules,
            }
        }

        async fn fail(&self, index: usize, error: HealthProbeError) {
            let module = &self.modules[index];
            // No monitor or OS process is started. Exercise the real health
            // decision and drain path, observing restart at scheduled respawn.
            handle_health_probe_failure(
                &module.spec,
                &module.runtime,
                &self.supervisor.registry,
                &self.supervisor.process_liveness,
                &module.handle.inner.snapshot,
                &mut None,
                error,
                42,
            )
            .await;
        }

        async fn silence(&self, index: usize) {
            self.fail(
                index,
                HealthProbeError::no_answer("injected probe deadline"),
            )
            .await;
        }

        fn assert_held(&self, index: usize) {
            let module = &self.modules[index];
            let status = module.handle.status().unwrap();
            assert_eq!(status.state, ModuleState::Running);
            assert!(status.process_alive);
            assert_eq!(status.health.status, SupervisorHealthStatus::Degraded);
            let detail = status.health.detail.as_deref().unwrap();
            assert!(detail.starts_with("[no-answer]"), "{detail}");
            assert!(detail.contains("host overloaded"), "{detail}");
            assert_eq!(status.health.last_probe_ms, Some(42));
            assert_eq!(status.health.consecutive_failures, 0);
            assert_eq!(status.restart_count, 0);
            assert_eq!(status.lifetime_restarts, 0);
            assert!(status.health.last_action.is_none());
            assert!(module.runtime.scheduled_respawn.lock().unwrap().is_none());
        }

        fn assert_restarted(&self, index: usize) {
            let module = &self.modules[index];
            let status = module.handle.status().unwrap();
            assert_eq!(status.state, ModuleState::Restarting);
            assert_eq!(status.restart_count, 1);
            assert_eq!(status.lifetime_restarts, 1);
            assert_eq!(status.health.last_action.as_deref(), Some("restart"));
            assert!(module.runtime.scheduled_respawn.lock().unwrap().is_some());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn four_silent_modules_hold_restarts_and_explain_each_probe() {
        for (count, running) in [(4, 4), (21, 13)] {
            let fleet = Fleet::new(count);
            for index in 13..count {
                let mut state = fleet.modules[index].handle.inner.snapshot.lock().unwrap();
                if index < 17 {
                    state.spawned_protocol = Some(ModuleProtocol::None);
                } else {
                    state.process_alive = false;
                }
            }
            for index in 0..4 {
                fleet.silence(index).await;
                if index < 2 || (running == 13 && index == 2) {
                    assert!(!fleet.handle.host_health.0.lock().unwrap().held);
                }
            }
            for index in 0..4 {
                // Includes failures recorded before the shared signal crossed
                // the threshold, not just the module that crossed it.
                fleet.assert_held(index);
                for _ in 0..=fleet.modules[index].runtime.health.failure_threshold {
                    fleet.silence(index).await;
                }
                fleet.assert_held(index);
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn isolated_silent_module_still_restarts_at_its_threshold() {
        let fleet = Fleet::new(1);
        let threshold = fleet.modules[0].runtime.health.failure_threshold;
        for failures in 1..threshold {
            fleet.silence(0).await;
            let status = fleet.modules[0].handle.status().unwrap();
            assert_eq!(status.state, ModuleState::Running);
            assert_eq!(status.health.status, SupervisorHealthStatus::Unknown);
            assert_eq!(status.health.consecutive_failures, failures);
            assert!(!status.health.detail.unwrap().contains("host overloaded"));
        }
        fleet.silence(0).await;
        fleet.assert_restarted(0);
    }

    #[tokio::test(start_paused = true)]
    async fn dead_lane_still_restarts_during_a_host_hold() {
        let fleet = Fleet::new(4);
        for index in 0..4 {
            fleet.silence(index).await;
        }
        fleet.assert_held(0);
        let threshold = fleet.modules[0].runtime.health.failure_threshold;
        for failures in 1..=threshold {
            fleet
                .fail(0, HealthProbeError::lane_dead("injected closed lane"))
                .await;
            let status = fleet.modules[0].handle.status().unwrap();
            assert_eq!(status.health.consecutive_failures, failures);
            assert!(status.health.detail.unwrap().starts_with("[lane-dead]"));
        }
        fleet.assert_restarted(0);
        fleet.assert_held(1);
    }

    #[tokio::test(start_paused = true)]
    async fn late_daemon_tick_holds_a_single_silent_module() {
        let fleet = Fleet::new(1);
        let now = Instant::now();
        fleet
            .handle
            .host_health
            .record_tick(now - Duration::from_secs(2), now);
        assert!(!fleet.handle.host_health.0.lock().unwrap().held);
        let monitor = fleet.handle.spawn_host_health_monitor();
        // The 1 s tick is not polled until the paused clock reaches 4 s.
        // This is scheduler delay, with no real sleep or CPU load.
        tokio::time::advance(Duration::from_secs(4)).await;
        for _ in 0..100 {
            if fleet.handle.host_health.0.lock().unwrap().held {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            fleet.handle.host_health.0.lock().unwrap().lag_exceeded,
            Some((Instant::now(), Duration::from_secs(3)))
        );
        for _ in 0..=fleet.modules[0].runtime.health.failure_threshold {
            fleet.silence(0).await;
        }
        fleet.assert_held(0);
        monitor.abort();
        assert!(monitor.await.unwrap_err().is_cancelled());
        tokio::time::advance(Duration::from_secs(59)).await;
        fleet.silence(0).await;
        fleet.assert_held(0);
        tokio::time::advance(Duration::from_secs(1)).await;
        fleet.silence(0).await;
        assert_eq!(
            fleet.modules[0]
                .handle
                .status()
                .unwrap()
                .health
                .consecutive_failures,
            1,
            "daemon lag must expire after one window"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cleared_host_hold_counts_only_new_failures_then_restarts() {
        use tracing_subscriber::prelude::*;
        let events = Events::default();
        // Keep a second log subscriber alive so a parallel test without log
        // capture cannot disable the log events this capture needs.
        let (_text, _interest_guard) = crate::router::test_log::log_capture(tracing::Level::INFO);
        let subscriber = tracing_subscriber::registry().with(events.clone());
        let _log_guard = tracing::subscriber::set_default(subscriber);
        let fleet = Fleet::new(4);
        fleet.silence(0).await;
        fleet.silence(0).await;
        // Late replies feed the same distinct-module window as no-answer
        // failures, while keeping the existing proof-of-life streak reset.
        for index in [1, 2, 3] {
            assert!(fleet
                .handle
                .record_late_health_answer(&fleet.modules[index].spec.module_id, 8_000)
                .unwrap());
        }
        fleet.assert_held(0);
        fleet.silence(0).await;
        tokio::time::advance(Duration::from_secs(59)).await;
        fleet.silence(0).await;
        fleet.assert_held(0);
        tokio::time::advance(Duration::from_secs(1)).await;
        let threshold = fleet.modules[0].runtime.health.failure_threshold;
        for failures in 1..=threshold {
            fleet.silence(0).await;
            let status = fleet.modules[0].handle.status().unwrap();
            assert_eq!(status.health.consecutive_failures, failures);
            assert!(!status.health.detail.unwrap().contains("host overloaded"));
            if failures < threshold {
                assert_eq!(status.state, ModuleState::Running);
                assert_eq!(status.lifetime_restarts, 0);
            }
        }
        fleet.assert_restarted(0);
        let events = events.0.lock().unwrap();
        for message in [
            "host overloaded; holding no-answer health restarts",
            "host overload cleared; resuming no-answer health restarts",
        ] {
            let transitions: Vec<_> = events
                .iter()
                .filter(|fields| fields.get("message").is_some_and(|s| s.contains(message)))
                .collect();
            assert_eq!(transitions.len(), 1, "{message}: {events:?}");
            assert!(transitions[0].contains_key("late_modules"));
            assert!(transitions[0].contains_key("daemon_lag_ms"));
        }
    }
}
