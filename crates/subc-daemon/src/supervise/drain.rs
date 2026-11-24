use super::*;

/// How long a drain waits for already-dispatched requests to finalize before
/// the child is torn down. Sized for TOOL-SCALE work (bash, inspect, builds),
/// not RPC-scale: the original 2s value silently cut nearly every real tool
/// call at the fence, making the wait-for-finalize design decorative for the
/// workloads it existed for. Quiescence short-circuits, so an idle module
/// restarts immediately regardless of this value; the budget is spent only
/// when a genuine in-flight request is worth finishing. Per-module override:
/// `drain_timeout_ms` in subc.jsonc; per-restart override: the operator's
/// `supervisor.restart{drain_timeout_ms}` (0 = cut now, for wedge bounces
/// where a stuck request will never settle).
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a drain waits after one busy-gauge probe completes before it asks
/// again.
///
/// A drain re-probes on this short interval of its own rather than on the
/// module's health cadence. The default cadence (30 s) equals the default
/// drain ceiling, so a cadence-paced drain that found the module busy at its
/// first probe never asked a second time: it waited out the whole ceiling and
/// forced GOODBYE even after the gauges had read zero. The interval is counted
/// from the moment the previous probe finished, and probes run one at a time,
/// so a slow module is never asked more often than it can answer. Periodic
/// health checks outside a drain keep using the configured cadence.
const DRAIN_BUSY_REPROBE_INTERVAL: Duration = Duration::from_millis(250);

impl SupervisedChild {
    pub(super) fn group_drain_budget(&self, budget: Duration, _notice: StopNotice) -> Duration {
        #[cfg(unix)]
        if self.child.plain_group.is_some() && _notice != StopNotice::SentOverConnection {
            return budget.min(plain_group::TERM_GRACE);
        }
        budget
    }
}

impl Supervisor {
    /// The flag [`Self::begin_daemon_shutdown`] sets, for the control handler's
    /// capability-requirement log to read.
    pub(crate) fn daemon_shutdown_flag(&self) -> crate::child_roster::DaemonShutdownFlag {
        self.child_roster.shutdown_flag()
    }

    /// The first step of an announced daemon shutdown, before the notice and
    /// before any connection is closed.
    ///
    /// Sets the daemon-shutdown flag first: from here on no module is
    /// respawned (crash restart, operator restart, or swap), and every child
    /// exit is recorded as `daemon_shutdown` rather than as a crash, whether
    /// the module exits on the EOF this shutdown gives it or is signalled by a
    /// service manager that kills the whole cgroup. Queues the journal marker
    /// without waiting for disk I/O.
    #[cfg(unix)]
    pub(crate) fn begin_daemon_shutdown(&self) {
        self.child_roster.close();
        if let Some(journal) = &self.terminal_journal {
            journal.stamp_shutdown();
        }
    }

    /// Announce a cut while established connections can still carry replies.
    /// These budgets bound the attempt to deliver notices and the wait for
    /// routes to drain; neither guarantees delivery or child completion, and
    /// they are local policy, not an estimate of launchd's unknown kill ceiling.
    #[cfg(unix)]
    pub(crate) async fn drain_for_daemon_shutdown(&self) -> Result<(), SuperviseError> {
        const NOTICE_BUDGET: Duration = Duration::from_millis(500);
        const DRAIN_BUDGET: Duration = Duration::from_secs(2);
        const MARKER_BUDGET: Duration = Duration::from_millis(500);
        let Some(forwarding) = &self.forwarding else {
            return Ok(());
        };
        // `begin_daemon_shutdown` only queued the shutdown marker. Let it reach
        // the journal before any module hears the notice, so a module that
        // looks at the journal on the notice already finds it. A stalled
        // writer costs at most MARKER_BUDGET; the notice goes out regardless.
        if let Some(journal) = &self.terminal_journal {
            if !journal.flush_within(MARKER_BUDGET).await {
                warn!(
                    "shutdown marker not written within its budget; announcing the shutdown anyway"
                );
            }
        }
        let notice_started = Instant::now();
        let modules = forwarding
            .begin_daemon_drain()
            .map_err(SuperviseError::Forwarding)?;
        let deadline_ms =
            unix_ms_now().saturating_add((NOTICE_BUDGET + DRAIN_BUDGET).as_millis() as u64);
        let shutdown_reason = self
            .supervisor_handle
            .as_ref()
            .map(SupervisorHandle::take_daemon_shutdown_reason)
            .unwrap_or(RouteCloseReason::Disable);
        let mut modules_told = 0;
        let mut notices = tokio::task::JoinSet::new();
        let mut drains = Vec::new();
        for connection in forwarding
            .module_connections()
            .map_err(SuperviseError::Forwarding)?
        {
            let target = ModuleDrainTarget {
                endpoint: connection.endpoint,
                sink: connection.sink,
                negotiated_ver: connection.negotiated_ver,
                abandoned_bindings: Vec::new(),
                excluded_subscriptions: 0,
            };
            notices.spawn(async move {
                (
                    true,
                    send_module_draining(
                        &connection.module_id,
                        shutdown_reason,
                        deadline_ms,
                        &target,
                        true,
                    )
                    .await,
                )
            });
        }
        for (module_id, endpoint) in modules {
            let Some(target) = forwarding
                .begin_endpoint_drain(endpoint, RouteCloseReason::Restart)
                .map_err(SuperviseError::Forwarding)?
            else {
                continue;
            };
            let routes = forwarding
                .endpoint_routes(target.endpoint)
                .map_err(SuperviseError::Forwarding)?;
            // Only an announced restart promises that modules are coming back.
            // Client route notices retain their existing reconnect semantics.
            let mut recipients = Vec::new();
            let mut clients: Vec<(crate::forwarding::GoodbyeTarget, Vec<u16>)> = Vec::new();
            for route in routes {
                let client = route.goodbye_target;
                if let Some((_, channels)) = clients
                    .iter_mut()
                    .find(|(existing, _)| existing.connection_id == client.connection_id)
                {
                    channels.push(client.channel);
                } else {
                    let channel = client.channel;
                    clients.push((client, vec![channel]));
                }
            }
            for (client, mut channels) in clients {
                channels.sort_unstable();
                channels.dedup();
                let closing = serde_json::to_vec(&ClientControlPush::RouteClosing {
                    module_id: module_id.clone(),
                    channels,
                    reason: RouteCloseReason::Restart,
                })
                .expect("route closing serializes");
                recipients.push((client.sink, client.negotiated_ver, closing));
            }
            for (sink, version, body) in recipients {
                notices.spawn(async move {
                    let frame = Frame::build_with_version(
                        version,
                        FrameType::Push,
                        control_flags(),
                        0,
                        0,
                        0,
                        body,
                    )
                    .expect("bounded lifecycle notice frame builds");
                    (false, sink.send_flushed(frame).await.is_ok())
                });
            }
            let gauges = declared_busy_gauges(&self.registry, &module_id)?;
            drains.push((module_id, target.endpoint, gauges));
        }
        // A quiet forwarding table is not proof that queued notices reached the
        // socket. Wait for writer flush acknowledgements before testing quiescence.
        let notice_deadline = Instant::now() + NOTICE_BUDGET;
        while let Ok(Some(result)) = timeout_at(notice_deadline, notices.join_next()).await {
            match result {
                Ok((true, true)) => modules_told += 1,
                Ok((false, true)) => {}
                result => warn!(?result, "daemon shutdown notice delivery failed"),
            }
        }
        notices.abort_all();
        info!(
            ?shutdown_reason,
            modules_told, "daemon shutdown module notices delivered"
        );
        let notice_elapsed = notice_started.elapsed();
        let drain_started = Instant::now();
        let deadline = drain_started + DRAIN_BUDGET;
        let mut waits = tokio::task::JoinSet::new();
        for (module_id, endpoint, gauges) in drains {
            let forwarding = Arc::clone(forwarding);
            let mut runtime = self.runtime_config();
            runtime.health.cadence = Duration::from_millis(100);
            waits.spawn(async move {
                let wait_result = wait_for_forwarding_quiescence(
                    &forwarding,
                    &module_id,
                    &runtime,
                    endpoint,
                    deadline,
                    &gauges,
                    DrainScope::Endpoint(endpoint),
                    &mut DrainGaugeTrace::new(&gauges),
                )
                .await;
                // Store the measurement before the task returns: quiescence
                // may be reached just as the deadline stops the join loop from
                // collecting results. Unfinished or failed waits report false.
                forwarding
                    .record_daemon_drain_result(
                        endpoint,
                        drained_after_quiescence_wait(&wait_result),
                    )
                    .map_err(SuperviseError::Forwarding)?;
                wait_result
            });
        }
        while let Ok(Some(result)) = timeout_at(deadline, waits.join_next()).await {
            if !matches!(result, Ok(Ok(true))) {
                warn!(?result, "daemon shutdown drain did not reach quiescence");
            }
        }
        // Measure only the forwarding drain, not child teardown or the caller's
        // signal delivery and process-reaping delays.
        info!(
            notice_elapsed_ms = notice_elapsed.as_millis() as u64,
            drain_elapsed_ms = drain_started.elapsed().as_millis() as u64,
            "daemon shutdown forwarding drain complete"
        );
        Ok(())
    }

    /// The last step of a graceful daemon shutdown, after the notice and the
    /// drain: send every registered module a module GOODBYE, the same planned
    /// stop signal `ck module stop` gives, then close every connection so each
    /// subc module sees EOF and starts its own teardown, then end every
    /// supervised child that has not exited
    /// by its own deadline (its drain budget, capped). Modules lead their own
    /// process groups, so a
    /// service manager's group kill no longer reaches them; without this a
    /// child that does not stop on EOF (every `protocol: "none"` child, which
    /// has no connection) would outlive the daemon. Every wait is bounded (see
    /// `child_roster`), and `escalate` resolving (a second SIGTERM) cuts them.
    #[cfg(unix)]
    pub(crate) async fn end_children_for_daemon_shutdown(
        &self,
        already_escalated: bool,
        escalate: impl std::future::Future<Output = ()>,
    ) {
        tokio::pin!(escalate);
        let mut escalated = already_escalated;
        if let Some(forwarding) = &self.forwarding {
            let reason = CloseReason::new(
                "daemon_shutdown",
                "the daemon is exiting after its shutdown notice and drain",
            );
            if escalated {
                // The operator asked to stop waiting: queue the GOODBYEs but
                // do not wait for them to be written.
                send_module_goodbyes_for_daemon_shutdown(forwarding, &reason, false).await;
            } else {
                tokio::select! {
                    biased;
                    _ = escalate.as_mut() => {
                        info!("second SIGTERM: abandoning module GOODBYE delivery");
                        escalated = true;
                    }
                    _ = send_module_goodbyes_for_daemon_shutdown(forwarding, &reason, true) => {}
                }
            }
            let closed = forwarding.close_all_connections(&reason);
            debug!(closed, "closed established connections for daemon shutdown");
        }
        // If the second SIGTERM arrived during GOODBYE delivery, `escalate` has
        // already completed and must not be polled again; the child shutdown
        // wait is told it is escalated and gets a future that never fires.
        let escalated_here = escalated && !already_escalated;
        let remaining_escalate = async move {
            if escalated_here {
                std::future::pending::<()>().await;
            } else {
                escalate.await;
            }
        };
        crate::child_roster::end_children_for_daemon_shutdown(
            &self.child_roster,
            escalated,
            remaining_escalate,
        )
        .await;
        if let Some(journal) = &self.terminal_journal {
            journal.flush_shutdown().await;
        }
    }
}

impl SupervisedModule {
    /// Drain the module and stop monitoring it.
    pub async fn drain(&self) -> Result<(), SuperviseError> {
        self.stop().await
    }

    pub(crate) async fn retire_requested(
        &self,
        requester: Option<Principal>,
    ) -> Result<(), SuperviseError> {
        match self.state()? {
            ModuleState::Stopped => return Ok(()),
            ModuleState::Failed
                if lock_snapshot(&self.inner.snapshot)?
                    .failed_retry
                    .pending
                    .is_none() =>
            {
                return Ok(())
            }
            ModuleState::Failed
            | ModuleState::Starting
            | ModuleState::Running
            | ModuleState::Unresponsive
            | ModuleState::Restarting
            | ModuleState::Draining
            | ModuleState::Disabled => {}
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(SupervisorCommand::Retire {
                requester,
                reply: reply_tx,
            })
            .await
            .map_err(|_| SuperviseError::CommandClosed {
                module_id: self.inner.module_id.clone(),
            })?;
        reply_rx.await.map_err(|_| SuperviseError::CommandClosed {
            module_id: self.inner.module_id.clone(),
        })?
    }

    pub async fn stop(&self) -> Result<(), SuperviseError> {
        self.stop_requested(None).await
    }

    pub(crate) async fn stop_requested(
        &self,
        requester: Option<Principal>,
    ) -> Result<(), SuperviseError> {
        match self.state()? {
            ModuleState::Stopped => return Ok(()),
            ModuleState::Failed
                if lock_snapshot(&self.inner.snapshot)?
                    .failed_retry
                    .pending
                    .is_none() =>
            {
                return Ok(())
            }
            ModuleState::Failed
            | ModuleState::Starting
            | ModuleState::Running
            | ModuleState::Unresponsive
            | ModuleState::Restarting
            | ModuleState::Draining
            | ModuleState::Disabled => {}
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(SupervisorCommand::Drain {
                requester,
                reply: reply_tx,
            })
            .await
            .map_err(|_| SuperviseError::CommandClosed {
                module_id: self.inner.module_id.clone(),
            })?;
        reply_rx.await.map_err(|_| SuperviseError::CommandClosed {
            module_id: self.inner.module_id.clone(),
        })?
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BusyGaugeObservation {
    Quiescent,
    Busy,
    Omitted,
}

/// What one health answer said about one declared busy gauge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum GaugeVerdict {
    /// The gauge read 0: this gauge is not what holds the drain.
    Zero,
    /// The gauge read a positive count.
    NonZero(u64),
    /// The gauge was present but not a non-negative integer (`0.0`, a string,
    /// a negative number). It counts as busy, because the daemon cannot tell
    /// it is idle. Carries the value as JSON text, cut to a short prefix.
    NotACount(String),
    /// The health answer had no metrics object, or one without this gauge.
    Missing,
    /// The probe produced no health answer to read; carries the probe error's
    /// evidence label (`no-answer` for a probe that timed out).
    ProbeFailed(&'static str),
}

impl GaugeVerdict {
    fn reads_quiet(&self) -> bool {
        matches!(self, Self::Zero)
    }
}

impl fmt::Display for GaugeVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero => f.write_str("zero"),
            Self::NonZero(value) => write!(f, "non_zero({value})"),
            Self::NotACount(value) => write!(f, "not_a_count({value})"),
            Self::Missing => f.write_str("missing_from_health_body"),
            Self::ProbeFailed(kind) => write!(f, "probe_failed({kind})"),
        }
    }
}

/// The longest JSON text kept for a gauge value that is not a count, so a
/// module reporting a large object as a gauge cannot flood the drain log.
const GAUGE_VALUE_TEXT_LIMIT: usize = 32;

/// Read each declared gauge out of one health answer's metrics.
pub(super) fn gauge_verdicts(metrics: Option<&Value>, gauges: &[String]) -> Vec<GaugeVerdict> {
    let metrics = metrics.and_then(Value::as_object);
    gauges
        .iter()
        .map(
            |gauge| match metrics.and_then(|metrics| metrics.get(gauge)) {
                None => GaugeVerdict::Missing,
                Some(value) => match value.as_u64() {
                    Some(0) => GaugeVerdict::Zero,
                    Some(count) => GaugeVerdict::NonZero(count),
                    None => {
                        let mut text = value.to_string();
                        if text.len() > GAUGE_VALUE_TEXT_LIMIT {
                            let mut end = GAUGE_VALUE_TEXT_LIMIT;
                            while !text.is_char_boundary(end) {
                                end -= 1;
                            }
                            text.truncate(end);
                            text.push_str("...");
                        }
                        GaugeVerdict::NotACount(text)
                    }
                },
            },
        )
        .collect()
}

/// The drain's overall reading of one health answer: omitted when any
/// declared gauge is missing, quiescent only when every gauge reads 0.
pub(super) fn busy_gauge_observation(verdicts: &[GaugeVerdict]) -> BusyGaugeObservation {
    if verdicts
        .iter()
        .any(|verdict| matches!(verdict, GaugeVerdict::Missing))
    {
        BusyGaugeObservation::Omitted
    } else if verdicts.iter().all(GaugeVerdict::reads_quiet) {
        BusyGaugeObservation::Quiescent
    } else {
        BusyGaugeObservation::Busy
    }
}

/// One declared gauge's history over a drain: the latest verdict on it, and
/// the latest value actually read for it (which a later failed probe or
/// missing gauge does not erase). Times are offsets from the start of the
/// drain's wait.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct GaugeTrace {
    pub(super) latest: Option<(GaugeVerdict, Duration)>,
    pub(super) last_value: Option<(GaugeVerdict, Duration)>,
}

/// What a drain's busy-gauge probes saw, kept so that a drain which runs to
/// its ceiling can say in its timeout line which declared gauge held it and
/// why. Without it, a gauge that read busy, a gauge the module never
/// reported, and a probe that never got an answer all looked the same: a
/// drain that ended at its ceiling with nothing in flight.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct DrainGaugeTrace {
    /// Health probes the drain sent.
    pub(super) probes: u32,
    /// Each declared gauge, in declaration order.
    pub(super) gauges: Vec<(String, GaugeTrace)>,
}

impl DrainGaugeTrace {
    pub(super) fn new(gauges: &[String]) -> Self {
        Self {
            probes: 0,
            gauges: gauges
                .iter()
                .map(|gauge| (gauge.clone(), GaugeTrace::default()))
                .collect(),
        }
    }

    /// Record one probe's verdicts, one per declared gauge, `at` into the drain.
    fn record(&mut self, verdicts: &[GaugeVerdict], at: Duration) {
        self.probes = self.probes.saturating_add(1);
        for ((_, trace), verdict) in self.gauges.iter_mut().zip(verdicts) {
            trace.latest = Some((verdict.clone(), at));
            if matches!(
                verdict,
                GaugeVerdict::Zero | GaugeVerdict::NonZero(_) | GaugeVerdict::NotACount(_)
            ) {
                trace.last_value = Some((verdict.clone(), at));
            }
        }
    }
}

/// Renders as `gauge: latest verdict @ offset; last value @ offset` for each
/// declared gauge, separated by ` | `. A value is shown as its verdict
/// (`zero`, `non_zero(n)`, `not_a_count(json)`), so it says both what was read
/// and whether that counted as busy.
impl fmt::Display for DrainGaugeTrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.gauges.is_empty() {
            return f.write_str("none declared");
        }
        for (index, (name, trace)) in self.gauges.iter().enumerate() {
            if index > 0 {
                f.write_str(" | ")?;
            }
            write!(f, "{name}: ")?;
            match &trace.latest {
                Some((verdict, at)) => write!(f, "{verdict} @{}ms", at.as_millis())?,
                None => f.write_str("never probed")?,
            }
            match &trace.last_value {
                Some((value, at)) => write!(f, "; last value {value} @{}ms", at.as_millis())?,
                None => f.write_str("; no value ever read")?,
            }
        }
        Ok(())
    }
}

pub(crate) fn declared_busy_gauges(
    registry: &Registry,
    module_id: &str,
) -> Result<Vec<String>, SuperviseError> {
    busy_gauges_of(
        registry
            .get_module(module_id)
            .map_err(SuperviseError::Registry)?,
    )
}

/// [`declared_busy_gauges`] for the registration a connection holds, in any
/// slot: after cutover the incumbent is no longer the id's active
/// registration, and its own manifest is the one that names its gauges.
fn declared_busy_gauges_for_connection(
    registry: &Registry,
    connection_id: ConnectionId,
) -> Result<Vec<String>, SuperviseError> {
    busy_gauges_of(
        registry
            .get_module_by_connection(connection_id)
            .map_err(SuperviseError::Registry)?,
    )
}

fn busy_gauges_of(
    registration: Option<crate::registry::ModuleRegistration>,
) -> Result<Vec<String>, SuperviseError> {
    let Some(registration) = registration else {
        return Ok(Vec::new());
    };
    let Some(self_signals) = registration.manifest.self_signals else {
        return Ok(Vec::new());
    };

    let mut gauges = Vec::new();
    for declaration in self_signals {
        if declaration.kind != SelfSignalKind::Busy {
            continue;
        }
        match declaration.anchored_to {
            SignalAnchor::HealthGauges { gauges: declared } if !declared.is_empty() => {
                gauges.extend(declared)
            }
            _ => {
                // An invalid Busy anchor is fail-safe: the empty name cannot be
                // present in a conforming health report, so this drain stays busy.
                gauges.push(String::new());
            }
        }
    }
    Ok(gauges)
}

/// Wait for `endpoint` to have nothing in flight and, when the module declares
/// busy gauges, for a health probe to report them quiet. The probe is addressed
/// by `scope`: a swap's superseded incumbent must be asked about its own
/// gauges, and by module id the probe would reach the promoted candidate.
/// While the gauges read busy, the probe repeats every
/// [`DRAIN_BUSY_REPROBE_INTERVAL`] until they read quiet or `deadline` passes.
/// Every probe's reading of each gauge is recorded in `trace`, for the caller's
/// timeout line.
#[allow(clippy::too_many_arguments)]
pub(super) async fn wait_for_forwarding_quiescence(
    forwarding: &ForwardingTable,
    module_id: &str,
    runtime: &SupervisorRuntimeConfig,
    endpoint: crate::ModuleEndpointId,
    deadline: Instant,
    busy_gauges: &[String],
    scope: DrainScope,
    trace: &mut DrainGaugeTrace,
) -> Result<bool, SuperviseError> {
    let wait_started = Instant::now();
    let mut gauges_quiescent = busy_gauges.is_empty();
    let mut next_probe_at = wait_started;
    let mut omission_reported = false;

    loop {
        let now = Instant::now();
        if !busy_gauges.is_empty() && now >= next_probe_at && now < deadline {
            let report = match scope {
                DrainScope::Active => probe_module_health(module_id, runtime, Some(deadline)).await,
                DrainScope::Endpoint(endpoint) => {
                    probe_endpoint_health(endpoint, runtime, Some(deadline)).await
                }
            };
            let verdicts = match &report {
                Ok(report) => gauge_verdicts(report.metrics.as_ref(), busy_gauges),
                Err(err) => vec![GaugeVerdict::ProbeFailed(err.label()); busy_gauges.len()],
            };
            trace.record(&verdicts, wait_started.elapsed());
            gauges_quiescent = match report {
                Ok(_) => match busy_gauge_observation(&verdicts) {
                    BusyGaugeObservation::Quiescent => true,
                    BusyGaugeObservation::Busy => false,
                    BusyGaugeObservation::Omitted => {
                        // Once per drain: a module that never reports a gauge
                        // it declared would otherwise say so on every probe.
                        if !omission_reported {
                            forwarding
                                .counters()
                                .increment_drains_with_undeclared_gauge();
                            let missing = busy_gauges
                                .iter()
                                .zip(&verdicts)
                                .filter(|(_, verdict)| matches!(verdict, GaugeVerdict::Missing))
                                .map(|(gauge, _)| gauge.as_str())
                                .collect::<Vec<_>>()
                                .join(",");
                            warn!(
                                module_id,
                                missing_gauges = %missing,
                                "drain health.check answer lacks a declared busy gauge; the drain treats the module as busy until it reports it"
                            );
                            omission_reported = true;
                        }
                        false
                    }
                },
                Err(err) => {
                    warn!(
                        module_id,
                        error = %err,
                        "drain health.check did not produce declared busy gauges; treating module as busy"
                    );
                    false
                }
            };
            // Counted from the probe's completion: the probe above was awaited,
            // so no second probe can start while one is still outstanding.
            next_probe_at = Instant::now() + drain_busy_reprobe_interval();
        }

        let in_flight = forwarding
            .endpoint_in_flight_count(endpoint)
            .map_err(SuperviseError::Forwarding)?;
        if in_flight == 0 && gauges_quiescent {
            return Ok(true);
        }

        let now = Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        let mut wait = deadline
            .saturating_duration_since(now)
            .min(REGISTRY_RELEASE_POLL);
        if !busy_gauges.is_empty() {
            wait = wait.min(next_probe_at.saturating_duration_since(now));
        }
        sleep(wait).await;
    }
}

/// The wait between drain busy-gauge probes: the larger of
/// [`DRAIN_BUSY_REPROBE_INTERVAL`] and [`REGISTRY_RELEASE_POLL`], the
/// drain's own in-flight poll interval.
pub(super) fn drain_busy_reprobe_interval() -> Duration {
    DRAIN_BUSY_REPROBE_INTERVAL.max(REGISTRY_RELEASE_POLL)
}

/// Logs why a drain gave up: the drain waited its full timeout without the
/// module going quiet, and teardown is being forced.
///
/// The line names two kinds of evidence, because a bare "timed out" hides
/// causes that need different fixes:
/// - `holdouts`: the requests and routes still open when the timeout hit,
///   and on which connections. One stuck call is a module bug; a held stream
///   on every session is a module that should end its streams when it
///   receives `module.draining`.
/// - `gauges`: for each busy gauge the module declares, its latest reading
///   and how many probes ran. This separates a module that kept reporting
///   itself busy, one whose health answer never contained the gauge, and one
///   that never answered the probes, which otherwise all look like a timeout
///   with nothing in flight.
pub(super) fn warn_drain_timed_out(
    module_id: &str,
    drain_timeout: Duration,
    reason: RouteCloseReason,
    holdouts: &crate::forwarding::DrainHoldouts,
    gauges: &DrainGaugeTrace,
) {
    warn!(
        module_id = %module_id,
        waited = ?drain_timeout,
        ?reason,
        held_requests = holdouts.requests,
        held_routes = holdouts.routes,
        total_routes = holdouts.total_routes,
        top_connections = ?holdouts.top_connections,
        // `module_channel:corr`, so the module can find each held request
        // in its own log; capped, so `held_requests` is the full count.
        held = %holdouts
            .held
            .iter()
            .map(|(channel, corr)| format!("{channel}:{corr}"))
            .collect::<Vec<_>>()
            .join(","),
        gauge_probes = gauges.probes,
        busy_gauges = %gauges,
        "route drain timed out before request quiescence; forcing teardown"
    );
}

/// The `route.closed` `drained` value implied by a quiescence-wait outcome.
///
/// `Ok` is always honest and passed straight through -- the wait actually measured
/// in-flight state. `Err` means the wait produced no measurement at all (the
/// forwarding table's lock was poisoned), so `false` is reported as the one honest
/// constant: the drain did not complete. Never recomputed from route state, never a
/// third "unknown" value -- the caller must still send a well-formed `route.closed`.
pub(super) fn drained_after_quiescence_wait(wait_result: &Result<bool, SuperviseError>) -> bool {
    match wait_result {
        Ok(drained) => *drained,
        Err(_) => false,
    }
}

fn send_route_goodbyes(forwarding: &ForwardingTable, released_routes: Vec<GoodbyeTarget>) {
    for released in released_routes {
        let frame = match Frame::build_with_version(
            released.negotiated_ver,
            FrameType::Goodbye,
            control_flags(),
            released.channel,
            released.epoch,
            0,
            Vec::new(),
        ) {
            Ok(frame) => frame,
            Err(err) => {
                warn!(
                    route_channel = released.channel,
                    error = %err,
                    "failed to build supervisor drain route GOODBYE frame"
                );
                continue;
            }
        };
        if !released.close_on_delivery_failure() {
            crate::forwarding::send_module_route_goodbye(
                &forwarding.counters(),
                &released.sink,
                frame,
                released.module_id.as_deref(),
                "supervisor drain",
            );
            continue;
        }
        if let Err(err) = released.sink.try_send(frame) {
            warn!(
                target_connection_id = released.connection_id.get(),
                route_channel = released.channel,
                error = %err,
                "supervisor drain route GOODBYE was not delivered to client; closing target connection"
            );
            let _ = forwarding.escalate_client_delivery_failure(
                released.connection_id,
                released.channel,
                released.epoch,
                CloseReason::new(
                    "route_goodbye_delivery_failed",
                    format!(
                        "failed to enqueue supervisor drain route GOODBYE for channel {}: {err}",
                        released.channel
                    ),
                ),
                crate::forwarding::UndeliveredFrame {
                    module_id: released.module_id.as_deref(),
                    sink: &released.sink,
                },
            );
        }
    }
}

async fn send_module_draining(
    module_id: &str,
    reason: RouteCloseReason,
    deadline_ms: u64,
    target: &ModuleDrainTarget,
    _wait_for_flush: bool,
) -> bool {
    let body = match serde_json::to_vec(&ModuleControlCommand::Draining {
        reason,
        deadline_ms,
    }) {
        Ok(body) => body,
        Err(err) => {
            warn!(
                module_id,
                error = %err,
                "failed to encode module draining command"
            );
            return false;
        }
    };
    let frame = match Frame::build_with_version(
        target.negotiated_ver,
        FrameType::Push,
        control_flags(),
        0,
        0,
        0,
        body,
    ) {
        Ok(frame) => frame,
        Err(err) => {
            warn!(
                module_id,
                error = %err,
                "failed to build module draining command frame"
            );
            return false;
        }
    };
    #[cfg(unix)]
    if _wait_for_flush {
        return target.sink.send_flushed(frame).await.is_ok();
    }
    if let Err(err) = target.sink.try_send(frame) {
        warn!(
            module_id,
            target_connection_id = target.endpoint.connection_id.get(),
            error = %err,
            "module draining command was not delivered to peer"
        );
        return false;
    }
    true
}

/// The channel-0 GOODBYE that tells a module its stop is planned.
fn module_goodbye_frame(module_id: &str, negotiated_ver: u8) -> Option<Frame> {
    match Frame::build_with_version(
        negotiated_ver,
        FrameType::Goodbye,
        control_flags(),
        0,
        0,
        0,
        Vec::new(),
    ) {
        Ok(frame) => Some(frame),
        Err(err) => {
            warn!(
                module_id,
                error = %err,
                "failed to build module GOODBYE frame"
            );
            None
        }
    }
}

/// Send every registered module connection its module GOODBYE at daemon
/// shutdown, then request that connection's close.
///
/// A module tells a planned stop from a lost daemon by whether a GOODBYE came
/// before EOF, so the GOODBYE must reach the socket before the close. A close
/// request does not wait for the connection's queued frames: its writer gets a
/// bounded grace after the close, is aborted if it overruns it, and the daemon
/// process may exit before that grace ends. So with `wait_for_flush`, each
/// connection is closed only after its writer has acknowledged writing the
/// GOODBYE, or once a short shared budget runs out, so one module that is not
/// reading cannot hold up the others or the shutdown. Without it the GOODBYEs
/// are only queued, for a shutdown the operator has told to stop waiting.
/// A connection that is already gone is skipped.
#[cfg(unix)]
async fn send_module_goodbyes_for_daemon_shutdown(
    forwarding: &Arc<ForwardingTable>,
    reason: &CloseReason,
    wait_for_flush: bool,
) {
    const GOODBYE_BUDGET: Duration = Duration::from_millis(500);
    let targets = match forwarding.module_connections() {
        Ok(targets) => targets,
        Err(err) => {
            warn!(error = %err, "could not list module connections for shutdown GOODBYE");
            return;
        }
    };
    let deadline = Instant::now() + GOODBYE_BUDGET;
    let mut sends = tokio::task::JoinSet::new();
    for target in targets {
        let Some(frame) = module_goodbye_frame(&target.module_id, target.negotiated_ver) else {
            continue;
        };
        if !wait_for_flush {
            if let Err(err) = target.sink.try_send(frame) {
                debug!(
                    module_id = %target.module_id,
                    error = %err,
                    "shutdown module GOODBYE was not queued"
                );
            }
            continue;
        }
        let forwarding = Arc::clone(forwarding);
        let reason = reason.clone();
        sends.spawn(async move {
            match timeout_at(deadline, target.sink.send_flushed(frame)).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => debug!(
                    module_id = %target.module_id,
                    error = %err,
                    "module connection closed before its shutdown GOODBYE was written"
                ),
                Err(_) => warn!(
                    module_id = %target.module_id,
                    budget = ?GOODBYE_BUDGET,
                    "shutdown module GOODBYE was not written within its budget; closing anyway"
                ),
            }
            forwarding.request_connection_close(target.endpoint.connection_id, reason);
        });
    }
    // Every task ends by the shared deadline, so this wait is bounded too.
    while sends.join_next().await.is_some() {}
}

async fn send_module_goodbye(
    module_id: &str,
    forwarding: &ForwardingTable,
    target: &ModuleDrainTarget,
    wait_for_flush: bool,
) {
    let Some(frame) = module_goodbye_frame(module_id, target.negotiated_ver) else {
        return;
    };
    // A channel is shut down synchronously after its drain. Its final frame
    // must reach the socket writer first; queue admission alone can lose it.
    let sent = if wait_for_flush {
        timeout(Duration::from_millis(500), target.sink.send_flushed(frame))
            .await
            .map_err(|_| "module GOODBYE flush exceeded 500ms".to_owned())
            .and_then(|result| result.map_err(|err| err.to_string()))
    } else {
        target.sink.try_send(frame).map_err(|err| err.to_string())
    };
    if let Err(err) = sent {
        warn!(
            module_id,
            target_connection_id = target.endpoint.connection_id.get(),
            error = %err,
            "supervisor drain module GOODBYE was not delivered to peer; closing module connection"
        );
        forwarding.request_connection_close(
            target.endpoint.connection_id,
            CloseReason::new(
                "module_goodbye_delivery_failed",
                format!("failed to enqueue supervisor drain module GOODBYE for module '{module_id}': {err}"),
            ),
        );
    }
}

#[derive(Clone, Copy)]
pub(super) struct ForwardingDrainContext<'a> {
    pub(super) spec: &'a ModuleSpec,
    pub(super) runtime: &'a SupervisorRuntimeConfig,
    pub(super) registry: &'a Registry,
    pub(super) scope: DrainScope,
}

/// Which process a forwarding drain addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DrainScope {
    /// Whatever endpoint is active for the module id: every plain stop,
    /// restart and reload. Also moves the module's state to `Draining`.
    Active,
    /// One specific endpoint: a swap's incumbent after cutover. Draining it by
    /// module id would resolve to the promoted candidate and leave neither
    /// process routable. The module's state is left alone, since the promoted
    /// candidate is what it describes and that process is running.
    Endpoint(crate::ModuleEndpointId),
}

/// Whether a child being drained has already been asked to stop by the time
/// its drain wait starts.
///
/// The drain wait is the same budget whatever this says. What it decides is
/// whether the supervisor must ask by signal before that wait begins: a child
/// that nobody asked will sit out the whole budget and then be SIGKILLed,
/// healthy or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StopNotice {
    /// The module was sent `module.draining` and a module GOODBYE over its own
    /// registered connection, and stops itself.
    SentOverConnection,
    /// The forwarding drain found no registered connection for the module: a
    /// subc child spawned moments ago that has not sent HELLO yet, or a
    /// `protocol: "none"` child, which never registers.
    NoConnection,
    /// This path sends nothing over the module's connection: the supervisor has
    /// no forwarding table, or the caller stops the child without a forwarding
    /// drain.
    NotSent,
}

pub(super) async fn begin_forwarding_drain(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    snapshot: &SharedSnapshot,
    enabled: Option<bool>,
    reason: RouteCloseReason,
) -> Result<StopNotice, SuperviseError> {
    let Some(forwarding) = runtime.forwarding.as_ref() else {
        return Err(SuperviseError::ReloadUnavailable {
            module_id: spec.module_id.clone(),
            reason: "supervisor was not configured with a forwarding table".to_string(),
        });
    };

    begin_forwarding_drain_with(
        forwarding,
        ForwardingDrainContext {
            spec,
            runtime,
            registry,
            scope: DrainScope::Active,
        },
        snapshot,
        enabled,
        reason,
        runtime.drain_timeout,
    )
    .await
}

pub(super) async fn begin_forwarding_drain_if_configured(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    snapshot: &SharedSnapshot,
    enabled: Option<bool>,
    reason: RouteCloseReason,
) -> Result<StopNotice, SuperviseError> {
    begin_forwarding_drain_with_timeout(
        spec,
        runtime,
        registry,
        snapshot,
        enabled,
        reason,
        runtime.drain_timeout,
    )
    .await
}

/// Like [`begin_forwarding_drain_if_configured`] but with an explicit drain
/// budget, for paths where the operator overrides the module's configured one
/// (`supervisor.restart{drain_timeout_ms}`).
pub(super) async fn begin_forwarding_drain_with_timeout(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    snapshot: &SharedSnapshot,
    enabled: Option<bool>,
    reason: RouteCloseReason,
    drain_timeout: Duration,
) -> Result<StopNotice, SuperviseError> {
    let Some(forwarding) = runtime.forwarding.as_ref() else {
        return Ok(StopNotice::NotSent);
    };

    begin_forwarding_drain_with(
        forwarding,
        ForwardingDrainContext {
            spec,
            runtime,
            registry,
            scope: DrainScope::Active,
        },
        snapshot,
        enabled,
        reason,
        drain_timeout,
    )
    .await
}

pub(super) async fn begin_forwarding_drain_with(
    forwarding: &ForwardingTable,
    context: ForwardingDrainContext<'_>,
    snapshot: &SharedSnapshot,
    enabled: Option<bool>,
    reason: RouteCloseReason,
    drain_timeout: Duration,
) -> Result<StopNotice, SuperviseError> {
    let ForwardingDrainContext {
        spec,
        runtime,
        registry,
        scope,
    } = context;
    debug_assert_ne!(reason, RouteCloseReason::Crash);
    let terminal = matches!(reason, RouteCloseReason::Disable);
    let drain_started_at = Instant::now();
    let drain_deadline = drain_started_at + drain_timeout;
    let deadline_ms =
        unix_ms_now().saturating_add(u64::try_from(drain_timeout.as_millis()).unwrap_or(u64::MAX));
    let busy_gauges = match scope {
        DrainScope::Active => declared_busy_gauges(registry, &spec.module_id)?,
        DrainScope::Endpoint(endpoint) => {
            declared_busy_gauges_for_connection(registry, endpoint.connection_id)?
        }
    };

    // Admission gate first: route.open/commit and route REQUEST admission are closed
    // before the first quiescence check, so the outstanding count can only fall.
    let gate_started = Instant::now();
    let drain_target = match scope {
        DrainScope::Active => forwarding.begin_module_drain(&spec.module_id, reason),
        DrainScope::Endpoint(endpoint) => forwarding.begin_endpoint_drain(endpoint, reason),
    }
    .map_err(SuperviseError::Forwarding)?;
    // The instant admission closed, and how long taking the forwarding write
    // lock to close it took. The timeout line reports only the quiescence
    // wait, so without this a drain that started late looked like one that
    // started on time.
    info!(
        module_id = %spec.module_id,
        ?reason,
        gate_ms = u64::try_from(gate_started.elapsed().as_millis()).unwrap_or(u64::MAX),
        connected = drain_target.is_some(),
        "module drain began; route admission closed"
    );
    if scope == DrainScope::Active {
        update_snapshot(snapshot, Some(&spec.module_id), |state| {
            state.state = ModuleState::Draining;
            state.draining_to_replace =
                matches!(reason, RouteCloseReason::Restart | RouteCloseReason::Reload);
            if let Some(enabled) = enabled {
                state.enabled = enabled;
            }
        })?;
    }

    let Some(target) = drain_target.as_ref() else {
        // Nothing was sent: the module has no registered connection to carry
        // `module.draining` or a GOODBYE. The caller must not assume the child
        // was asked to stop.
        return Ok(StopNotice::NoConnection);
    };
    let flush_goodbye = runtime.supervisor_handle.as_ref().is_some_and(|handle| {
        handle
            .registration_spawn(target.endpoint.connection_id)
            .is_some()
    });
    {
        send_module_draining(&spec.module_id, reason, deadline_ms, target, false).await;
        #[cfg(feature = "test-support")]
        forwarding.wait_before_route_closing_for_test().await;
        let routes = forwarding
            .endpoint_routes(target.endpoint)
            .map_err(SuperviseError::Forwarding)?;
        let routes_notified = routes.len();
        crate::control::send_route_control_pushes(
            forwarding,
            routes.clone(),
            ClientControlPush::RouteClosing {
                module_id: spec.module_id.clone(),
                channels: Vec::new(),
                reason,
            },
        );
        send_route_goodbyes(forwarding, target.abandoned_bindings.clone());

        // `route.closing` was just sent above: from here on every return path,
        // including an early one, MUST send `route.closed` before propagating
        // anything else. A client holds `closing` as a promise that a verdict is
        // coming; leaving early without `closed` strands it waiting forever, since
        // `closing` carries no timeout of its own.
        let mut gauge_trace = DrainGaugeTrace::new(&busy_gauges);
        let wait_result = wait_for_forwarding_quiescence(
            forwarding,
            &spec.module_id,
            runtime,
            target.endpoint,
            drain_deadline,
            &busy_gauges,
            scope,
            &mut gauge_trace,
        )
        .await;
        let drained = drained_after_quiescence_wait(&wait_result);
        if let Err(err) = &wait_result {
            error!(
                module_id = %spec.module_id,
                ?reason,
                error = %err,
                "forwarding quiescence wait failed after route.closing; forcing route.closed(drained: false) so the client is not left waiting on an unfulfilled promise"
            );
        } else if !drained {
            // Read before teardown releases the routes.
            let holdouts = forwarding
                .endpoint_drain_holdouts(target.endpoint)
                .unwrap_or_default();
            warn_drain_timed_out(
                &spec.module_id,
                drain_timeout,
                reason,
                &holdouts,
                &gauge_trace,
            );
        }
        crate::control::send_route_control_pushes(
            forwarding,
            routes,
            ClientControlPush::RouteClosed {
                module_id: spec.module_id.clone(),
                channels: Vec::new(),
                reason,
                drained,
                abandoned: target.abandoned_bindings.len() as u32,
                excluded_subscriptions: target.excluded_subscriptions,
                terminal: Some(terminal),
            },
        );
        wait_result?;

        // `route.closed` has now been sent unconditionally above. From here the
        // remaining steps are cleanup (route + module GOODBYE) rather than a
        // promise the client is waiting on, but a lock-poisoned
        // `release_module_endpoint_routes` would otherwise skip the module
        // GOODBYE silently too -- send it before propagating the error.
        let released_routes = match forwarding.release_module_endpoint_routes(target.endpoint) {
            Ok(routes) => routes,
            Err(err) => {
                warn!(
                    module_id = %spec.module_id,
                    ?reason,
                    error = %err,
                    "failed to release module endpoint routes after route.closed; module GOODBYE will still be sent"
                );
                send_module_goodbye(&spec.module_id, forwarding, target, flush_goodbye).await;
                return Err(SuperviseError::Forwarding(err));
            }
        };
        let route_goodbye_count = released_routes.len();
        send_route_goodbyes(forwarding, released_routes);
        send_module_goodbye(&spec.module_id, forwarding, target, flush_goodbye).await;

        // The drain's happy path was previously silent: every emission above is
        // best-effort with only its failure arm logged, so "were consumers told"
        // was unprovable from the daemon log (surfaced by a 30-minute consumer
        // hang where the open question was exactly whether teardown notice went
        // out). One summary line makes that class decidable in one grep.
        info!(
            module_id = %spec.module_id,
            ?reason,
            routes_notified,
            route_goodbyes = route_goodbye_count,
            abandoned_reservations = target.abandoned_bindings.len(),
            excluded_subscriptions = target.excluded_subscriptions,
            drained,
            "module drain complete; consumers notified via route.closing/route.closed pushes and per-route GOODBYE frames"
        );
    }

    Ok(StopNotice::SentOverConnection)
}

pub(super) fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn drain_optional_child(
    module_id: &str,
    protocol: ModuleProtocol,
    stop_notice: StopNotice,
    registry: &Registry,
    forwarding: Option<&ForwardingTable>,
    snapshot: &SharedSnapshot,
    terminal_ring: &Arc<Mutex<TerminalRing>>,
    spawn_events: &SpawnEventFeed,
    child: &mut Option<SupervisedChild>,
    drain_timeout: Duration,
    final_state: ModuleState,
    enabled: Option<bool>,
    failure: &FailureContext,
) -> Result<(), SuperviseError> {
    if let Some(child) = child.take() {
        drain_child_to_state(
            module_id,
            protocol,
            stop_notice,
            registry,
            forwarding,
            snapshot,
            terminal_ring,
            spawn_events,
            child,
            drain_timeout,
            final_state,
            enabled,
            failure,
        )
        .await
    } else {
        update_snapshot(snapshot, Some(module_id), |state| {
            state.state = final_state;
            if final_state == ModuleState::Failed {
                state.failed_retry.failed_at = Some(Instant::now());
            }
            if let Some(enabled) = enabled {
                state.enabled = enabled;
            }
            clear_current_process_facts(state);
        })?;
        release_dead_registration(registry, forwarding, snapshot, module_id, failure).await
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn drain_child_to_state(
    module_id: &str,
    _protocol: ModuleProtocol,
    stop_notice: StopNotice,
    registry: &Registry,
    forwarding: Option<&ForwardingTable>,
    snapshot: &SharedSnapshot,
    terminal_ring: &Arc<Mutex<TerminalRing>>,
    spawn_events: &SpawnEventFeed,
    mut child: SupervisedChild,
    drain_timeout: Duration,
    final_state: ModuleState,
    enabled: Option<bool>,
    failure: &FailureContext,
) -> Result<(), SuperviseError> {
    let protocol = child.protocol;
    child.retire_registration();
    update_snapshot(snapshot, Some(module_id), |state| {
        state.state = ModuleState::Draining;
        state.draining_to_replace = final_state == ModuleState::Restarting;
        if let Some(enabled) = enabled {
            state.enabled = enabled;
        }
    })?;

    // The wait below is the same budget in every case; what differs is
    // whether anything has ASKED the child to stop before it starts. Only a
    // forwarding drain that reached the module's registered connection has
    // (`module.draining`, then a module GOODBYE). Every other child was told
    // nothing: a `protocol: "none"` module, which never registers; a subc
    // module spawned moments ago that has not sent HELLO yet; or a stop that
    // runs no forwarding drain. Without a signal the budget is only a delay
    // in front of SIGKILL -- and the not-yet-registered child is the worst
    // case, because it registers into a module that is already draining,
    // is never told, and is killed while healthy.
    if stop_notice != StopNotice::SentOverConnection {
        if protocol == ModuleProtocol::Subc && stop_notice == StopNotice::NoConnection {
            info!(
                module_id,
                pid = child.pid,
                budget_ms = u64::try_from(drain_timeout.as_millis()).unwrap_or(u64::MAX),
                "module has no connection yet; requesting stop by signal"
            );
        }
        request_graceful_stop(module_id, &child);
    }

    let drain_timeout = child.group_drain_budget(drain_timeout, stop_notice);
    let exit_report = match timeout(drain_timeout, child.wait()).await {
        Ok(Ok(status)) => classify_reaped_child_exit(snapshot, &child, &status),
        Ok(Err(source)) => {
            fail_after_wait_error(
                module_id,
                snapshot,
                terminal_ring,
                spawn_events,
                format!("failed to wait during drain: {source}"),
                failure,
            );
            return Err(SuperviseError::Wait {
                module_id: module_id.to_string(),
                source,
            });
        }
        Err(_) => {
            // Mirror the sibling arm above: state is already `Draining`, and an
            // error propagated from here would strand it there -- a state
            // `set_enabled(true)` cannot heal (`revive_terminal` matches only
            // `Failed | Stopped`), leaving an operator Restart as the only exit.
            // `Failed` before `?` keeps the module operator-visible and
            // revivable. Trigger is an ESRCH race (process exits between the
            // drain timeout firing and the kill) or a post-kill wait failure
            // (issue #34).
            //
            // Logged because the kill is otherwise visible only as signal 9 in
            // the terminal ring, and the budget it follows can be long enough
            // that consumers see a stretch of refusals with no stated cause.
            warn!(
                module_id,
                pid = child.pid,
                budget_ms = u64::try_from(drain_timeout.as_millis()).unwrap_or(u64::MAX),
                reason = ?final_state,
                ?stop_notice,
                "drain budget expired before the module exited; killing it"
            );
            let kill =
                if final_state == ModuleState::Restarting && child.child.container().is_some() {
                    child.kill_leader()
                } else {
                    child.start_kill()
                };
            kill.map_err(|source| {
                fail_after_wait_error(
                    module_id,
                    snapshot,
                    terminal_ring,
                    spawn_events,
                    format!("failed to kill during drain: {source}"),
                    failure,
                );
                SuperviseError::Kill {
                    module_id: module_id.to_string(),
                    source,
                }
            })?;
            let status = child.wait().await.map_err(|source| {
                fail_after_wait_error(
                    module_id,
                    snapshot,
                    terminal_ring,
                    spawn_events,
                    format!("failed to wait after drain kill: {source}"),
                    failure,
                );
                SuperviseError::Wait {
                    module_id: module_id.to_string(),
                    source,
                }
            })?;
            classify_reaped_child_exit(snapshot, &child, &status)
        }
    };

    child.drain_stderr(module_id).await;
    let detail =
        child.exit_cleanup_detail(lock_snapshot(snapshot)?.drain_disposition_detail.take());
    publish_after_terminal_record(
        module_id,
        || {
            record_terminal_with_detail(
                module_id,
                terminal_ring,
                spawn_events,
                &exit_report,
                terminal_disposition(final_state),
                detail,
            )
        },
        || {
            update_snapshot(snapshot, Some(module_id), |state| {
                state.state = final_state;
                if final_state == ModuleState::Failed {
                    state.failed_retry.failed_at = Some(Instant::now());
                }
                if let Some(enabled) = enabled {
                    state.enabled = enabled;
                }
                clear_current_process_facts(state);
                state.last_exit = Some(exit_report.clone());
                if exit_report.kind == ExitKind::DeliberateSeverance {
                    state.lifetime_restarts += 1;
                }
            })
        },
    )?;
    let released =
        release_dead_registration(registry, forwarding, snapshot, module_id, failure).await;
    child
        .finish_exit(
            released.is_ok()
                && final_state == ModuleState::Restarting
                && child.spawn_failure.is_none(),
        )
        .await
        .map_err(|source| SuperviseError::Wait {
            module_id: module_id.to_owned(),
            source,
        })?;
    released
}

/// Ask a child that nothing else has asked to stop, by signal.
///
/// A registered subc module is asked over its own connection: the drain sends
/// `route.closing`/`route.closed` to its consumers, a GOODBYE per route, then a
/// module GOODBYE, and the module stops itself. A module that speaks no subc
/// wire receives none of that, and neither does a subc module that has not
/// registered yet, so for them the drain budget would be pure delay in front of
/// a SIGKILL -- and for a process with a store to flush (JetStream is the
/// reason `protocol: "none"` exists) a SIGKILL turns every ordinary teardown
/// into a recovery on the next start.
///
/// NEVER CALLED FOR A MODULE THAT WAS TOLD OVER ITS CONNECTION, and that is a
/// rule rather than an optimisation: that module's graceful stop is already
/// running by the time its child is drained, and a signal would race it.
///
/// Best-effort by construction. A child that has already exited is the ordinary
/// case rather than an error (the kill lands on a reaped or exiting pid), so a
/// failure is logged at debug and the wait-then-kill below still decides the
/// outcome.
#[cfg(unix)]
pub(super) fn request_graceful_stop(module_id: &str, child: &SupervisedChild) {
    if let Some(group) = &child.child.plain_group {
        if let Err(error) = group.signal(rustix::process::Signal::TERM) {
            debug!(module_id, %error, "group TERM was not delivered; the drain wait and kill still apply");
        }
        return;
    }
    let Some(pid) = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .filter(|pid| *pid > 1)
        .and_then(rustix::process::Pid::from_raw)
    else {
        debug!(
            module_id,
            "no pid to signal for teardown; falling through to the drain wait"
        );
        return;
    };
    match rustix::process::kill_process(pid, rustix::process::Signal::TERM) {
        Ok(()) => debug!(
            module_id,
            "sent SIGTERM to a module nothing else asked to stop"
        ),
        Err(err) => debug!(
            module_id,
            error = %err,
            "SIGTERM to module failed; the drain wait and kill still apply"
        ),
    }
}

/// Windows has no SIGTERM and no portable stand-in for one. The graceful stops
/// Windows does offer need cooperation this supervisor cannot assume: a console
/// control event requires sharing a console with the child, and `WM_CLOSE`
/// requires the child to pump a message loop. A supervised server process does
/// neither, so there is nothing to send and teardown is the wait followed by the
/// kill. Emulating a signal here would mean inventing a stop protocol, which is
/// the thing `protocol: "none"` exists to avoid.
#[cfg(not(unix))]
pub(super) fn request_graceful_stop(module_id: &str, _child: &SupervisedChild) {
    debug!(
        module_id,
        "no graceful stop signal exists on this platform; teardown of a module nothing asked to stop waits, then kills"
    );
}

pub(super) fn terminal_disposition(final_state: ModuleState) -> TerminalDisposition {
    match final_state {
        ModuleState::Stopped => TerminalDisposition::Stopped,
        ModuleState::Disabled => TerminalDisposition::Disabled,
        ModuleState::Restarting => TerminalDisposition::Restarting,
        ModuleState::Failed => TerminalDisposition::Failed,
        ModuleState::Starting
        | ModuleState::Running
        | ModuleState::Unresponsive
        | ModuleState::Draining => {
            unreachable!("terminal exits only finish in terminal or restarting states")
        }
    }
}
