//! The drain's busy-gauge wait, driven against an in-process module
//! connection whose health answers the test scripts. Time is paused, so
//! every elapsed duration below is exact virtual time.
use super::*;
use std::collections::BTreeMap;
use std::sync::Mutex as StdMutex;
use subc_protocol::{manifest::Concurrency, session::ModuleControlResponse};

const MODULE_ID: &str = "drain-reprobe-module";
const CONNECTION: u64 = 7;
const GAUGE: &str = "runs_in_flight";
/// The production default for both the health cadence and the drain
/// ceiling, which is the pairing that used to cost a full ceiling.
const CADENCE: Duration = Duration::from_secs(30);
const CEILING: Duration = Duration::from_secs(30);

/// How the scripted module answers each `health.check`.
#[derive(Clone, Copy)]
enum Answer {
    /// Report the gauge busy until this instant, quiet from then on, after
    /// taking `latency` to answer.
    BusyUntil { until: Instant, latency: Duration },
    /// Answer at once, with metrics that leave the declared gauge out.
    OmitsGauge,
    /// Never answer.
    Silent,
}

struct Drain {
    forwarding: Arc<ForwardingTable>,
    runtime: SupervisorRuntimeConfig,
    endpoint: crate::ModuleEndpointId,
    /// When each probe reached the module.
    probes: Arc<StdMutex<Vec<Instant>>>,
    responder: tokio::task::JoinHandle<()>,
}

impl Drain {
    fn start(answer: Answer) -> Self {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let health = HealthConfig {
            cadence: CADENCE,
            ..HealthConfig::default()
        };
        let runtime = Supervisor::new_for_test(registry, RestartPolicy::new(3, Duration::ZERO))
            .with_forwarding(forwarding.clone())
            .with_health_config(health)
            .runtime_config();
        let (tx, mut rx) = mpsc::channel::<crate::router::OutboundFrame>(8);
        let endpoint = forwarding
            .register_module_connection(
                ConnectionId::new(CONNECTION),
                MODULE_ID.to_string(),
                subc_protocol::PROTOCOL_VERSION,
                Concurrency::ModuleManaged,
                FrameSink::new(tx),
            )
            .unwrap();
        let probes = Arc::new(StdMutex::new(Vec::new()));
        let responder = tokio::spawn({
            let forwarding = forwarding.clone();
            let probes = probes.clone();
            async move {
                while let Some(frame) = rx.recv().await {
                    let body: Value = serde_json::from_slice(&frame.body).unwrap();
                    assert_eq!(body["op"], MODULE_CONTROL_OP_HEALTH_CHECK);
                    probes.lock().unwrap().push(Instant::now());
                    let metrics = match answer {
                        Answer::BusyUntil { until, latency } => {
                            sleep(latency).await;
                            let busy = u64::from(Instant::now() < until);
                            serde_json::json!({ GAUGE: busy })
                        }
                        Answer::OmitsGauge => serde_json::json!({ "queue_depth": 0 }),
                        Answer::Silent => continue,
                    };
                    // A probe abandoned at its deadline has no RPC left to
                    // complete, so a late answer may be refused.
                    let _ = forwarding.complete_module_control_rpc(
                        ConnectionId::new(CONNECTION),
                        frame.header.corr,
                        Some(MODULE_CONTROL_OP_HEALTH_CHECK),
                        ModuleControlRpcOutcome::Response(ModuleControlResponse::HealthCheck {
                            status: HealthStatus::Ok,
                            detail: None,
                            metrics: Some(metrics),
                        }),
                    );
                }
            }
        });
        Self {
            forwarding,
            runtime,
            endpoint,
            probes,
            responder,
        }
    }

    /// Run one by-id drain wait with `ceiling`, returning its verdict and
    /// how long it took.
    async fn run(&self, ceiling: Duration) -> (bool, Duration) {
        let (drained, elapsed, _) = self.run_traced(ceiling).await;
        (drained, elapsed)
    }

    /// [`Self::run`], also returning what the drain's probes read.
    async fn run_traced(&self, ceiling: Duration) -> (bool, Duration, DrainGaugeTrace) {
        let started = Instant::now();
        let mut trace = DrainGaugeTrace::new(&[GAUGE.to_string()]);
        let drained = wait_for_forwarding_quiescence(
            &self.forwarding,
            MODULE_ID,
            &self.runtime,
            self.endpoint,
            started + ceiling,
            &[GAUGE.to_string()],
            DrainScope::Active,
            &mut trace,
        )
        .await
        .unwrap();
        (drained, started.elapsed(), trace)
    }

    fn probes(&self) -> Vec<Instant> {
        self.probes.lock().unwrap().clone()
    }
}

impl Drop for Drain {
    fn drop(&mut self) {
        self.responder.abort();
    }
}

#[tokio::test(start_paused = true)]
async fn busy_at_first_probe_then_quiet_drains_long_before_the_ceiling() {
    let drain = Drain::start(Answer::BusyUntil {
        until: Instant::now() + Duration::from_secs(1),
        latency: Duration::ZERO,
    });
    let (drained, elapsed) = drain.run(CEILING).await;
    assert!(
        drained,
        "gauges that read zero during the drain must let it finish drained, \
             not wait out the {CADENCE:?} health cadence (elapsed {elapsed:?})"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "drain took {elapsed:?} after the gauges went quiet at 1s"
    );
    let probes = drain.probes();
    assert!(
        probes.len() >= 2,
        "the module was busy at the first probe, so a quiet verdict needs a second: {probes:?}"
    );
}

/// The same shape in real time on a multi-threaded runtime: the first
/// probe is answered busy at once, within a millisecond of the drain
/// starting, and later probes read idle. The drain must keep probing
/// after that first instant answer and end soon after the gauge falls.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_instant_busy_first_answer_does_not_stop_later_probes_in_real_time() {
    let drain = Drain::start(Answer::BusyUntil {
        until: Instant::now() + Duration::from_millis(600),
        latency: Duration::ZERO,
    });
    let (drained, elapsed, trace) = drain.run_traced(CEILING).await;
    assert!(drained, "the gauge fell at 600ms: {trace}");
    assert!(
        elapsed < Duration::from_secs(3),
        "drain took {elapsed:?} after the gauge fell at 600ms: {trace}"
    );
    let probes = drain.probes();
    assert!(
        probes.len() >= 2 && trace.probes as usize == probes.len(),
        "the drain must re-probe after the first busy answer: {probes:?}, {trace}"
    );
}

#[tokio::test(start_paused = true)]
async fn busy_throughout_ends_undrained_at_the_ceiling_with_bounded_probes() {
    let started = Instant::now();
    let drain = Drain::start(Answer::BusyUntil {
        until: started + Duration::from_secs(3600),
        latency: Duration::ZERO,
    });
    let (drained, elapsed) = drain.run(CEILING).await;
    assert!(!drained, "a module busy throughout must not drain");
    assert!(
        elapsed >= CEILING && elapsed < CEILING + drain_busy_reprobe_interval(),
        "a busy module is held until the ceiling and no longer: {elapsed:?}"
    );
    // Re-probing is paced by the interval, not a busy loop: at most one
    // probe per interval across the ceiling, plus the first at time zero.
    let probes = drain.probes();
    let max_probes = (CEILING.as_millis() / drain_busy_reprobe_interval().as_millis()) + 1;
    assert!(
        probes.len() as u128 <= max_probes,
        "{} probes in a {CEILING:?} drain exceeds the {max_probes} the interval allows",
        probes.len()
    );
    for pair in probes.windows(2) {
        assert!(
            pair[1] - pair[0] >= drain_busy_reprobe_interval(),
            "consecutive probes {:?} apart",
            pair[1] - pair[0]
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_slow_probe_is_never_overlapped_by_the_next() {
    // Each answer takes longer than the re-probe interval; the next probe
    // must wait for the previous answer and then the interval.
    let latency = Duration::from_secs(1);
    let drain = Drain::start(Answer::BusyUntil {
        until: Instant::now() + Duration::from_secs(3600),
        latency,
    });
    let (drained, _) = drain.run(Duration::from_secs(5)).await;
    assert!(!drained);
    let probes = drain.probes();
    assert!(probes.len() >= 2, "the drain must re-probe: {probes:?}");
    for pair in probes.windows(2) {
        assert!(
            pair[1] - pair[0] >= latency + drain_busy_reprobe_interval(),
            "a probe started {:?} after the previous one, before it could finish",
            pair[1] - pair[0]
        );
    }
}

#[tokio::test(start_paused = true)]
async fn an_unanswered_probe_cannot_hold_the_drain_past_its_ceiling() {
    let drain = Drain::start(Answer::Silent);
    let ceiling = Duration::from_secs(2);
    let (drained, elapsed) = drain.run(ceiling).await;
    assert!(!drained, "an unanswered probe counts as busy");
    assert!(
        elapsed >= ceiling && elapsed < ceiling + drain_busy_reprobe_interval(),
        "the probe deadline is capped at the drain ceiling: {elapsed:?}"
    );
    assert_eq!(
        drain.probes().len(),
        1,
        "the unanswered probe holds the only slot until the ceiling"
    );
}

/// Every event logged while it is installed, as `message` plus fields.
#[derive(Clone, Default)]
struct LogCapture {
    events: Arc<StdMutex<Vec<BTreeMap<String, String>>>>,
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for LogCapture {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        struct Fields(BTreeMap<String, String>);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
                self.0
                    .insert(field.name().to_string(), format!("{value:?}"));
            }
        }
        let mut fields = Fields(BTreeMap::new());
        event.record(&mut fields);
        self.events.lock().unwrap().push(fields.0);
    }
}

impl LogCapture {
    fn install(&self) -> tracing::subscriber::DefaultGuard {
        use tracing_subscriber::layer::SubscriberExt;
        tracing::subscriber::set_default(tracing_subscriber::registry().with(self.clone()))
    }

    fn with_message(&self, needle: &str) -> Vec<BTreeMap<String, String>> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|fields| fields.get("message").is_some_and(|m| m.contains(needle)))
            .cloned()
            .collect()
    }
}

/// A module that declares a busy gauge but leaves it out of its health
/// answers holds a drain to its ceiling. The timeout line must say so by
/// name, and the drain must warn about the missing gauge once, not on
/// every probe.
#[tokio::test(start_paused = true)]
async fn a_missing_declared_gauge_is_named_on_the_drain_timeout_line() {
    let capture = LogCapture::default();
    let _guard = capture.install();
    let drain = Drain::start(Answer::OmitsGauge);
    let (drained, _, trace) = drain.run_traced(Duration::from_secs(2)).await;
    assert!(!drained, "a gauge never reported cannot read quiet");
    assert!(trace.probes >= 2, "the drain must re-probe: {trace:?}");

    let missing = capture.with_message("lacks a declared busy gauge");
    assert_eq!(missing.len(), 1, "one warning per drain: {missing:?}");
    assert_eq!(
        missing[0].get("missing_gauges").map(String::as_str),
        Some(GAUGE)
    );

    warn_drain_timed_out(
        MODULE_ID,
        Duration::from_secs(2),
        RouteCloseReason::Restart,
        &Default::default(),
        &trace,
    );
    let timeouts = capture.with_message("route drain timed out");
    assert_eq!(timeouts.len(), 1, "{timeouts:?}");
    let gauges = &timeouts[0]["busy_gauges"];
    assert!(
        gauges.starts_with(&format!("{GAUGE}: missing_from_health_body @")),
        "the timeout line must name the gauge and why it read busy: {gauges}"
    );
    assert!(
        gauges.ends_with("; no value ever read"),
        "no value was ever read for the gauge: {gauges}"
    );
    assert_eq!(timeouts[0]["gauge_probes"], trace.probes.to_string());
}

/// The trace names a busy value and when it was read. For a probe that
/// never got an answer, it names the probe's error kind and says that no
/// value was ever read.
#[tokio::test(start_paused = true)]
async fn the_drain_trace_names_a_busy_value_and_an_unanswered_probe() {
    let busy = Drain::start(Answer::BusyUntil {
        until: Instant::now() + Duration::from_secs(3600),
        latency: Duration::ZERO,
    });
    let (_, _, trace) = busy.run_traced(Duration::from_secs(1)).await;
    let rendered = trace.to_string();
    assert!(
        rendered.starts_with(&format!("{GAUGE}: non_zero(1) @"))
            && rendered.contains("; last value non_zero(1) @"),
        "{rendered}"
    );

    let silent = Drain::start(Answer::Silent);
    let (_, _, trace) = silent.run_traced(Duration::from_secs(1)).await;
    // The probe is capped at the 1 s ceiling, and its reading is stamped
    // when it gave up.
    assert_eq!(
        trace.to_string(),
        format!("{GAUGE}: probe_failed(no-answer) @1000ms; no value ever read")
    );
    assert_eq!(trace.probes, 1);
}

/// A gauge value that is not a count reads busy and is shown as sent.
#[test]
fn a_gauge_that_is_not_a_count_is_busy_and_shown_as_sent() {
    let gauges = [GAUGE.to_string()];
    let verdicts = gauge_verdicts(Some(&serde_json::json!({ GAUGE: 0.0 })), &gauges);
    assert_eq!(verdicts, vec![GaugeVerdict::NotACount("0.0".to_string())]);
    assert_eq!(
        busy_gauge_observation(&verdicts),
        BusyGaugeObservation::Busy
    );
    let zero = gauge_verdicts(Some(&serde_json::json!({ GAUGE: 0 })), &gauges);
    assert_eq!(
        busy_gauge_observation(&zero),
        BusyGaugeObservation::Quiescent
    );
    let none = gauge_verdicts(None, &gauges);
    assert_eq!(busy_gauge_observation(&none), BusyGaugeObservation::Omitted);
}
