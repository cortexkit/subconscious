use super::*;

const DEFAULT_HEALTH_CADENCE: Duration = Duration::from_secs(30);
const DEFAULT_HEALTH_DEADLINE: Duration = Duration::from_secs(5);
const DEFAULT_HEALTH_FAILURE_THRESHOLD: u32 = 3;
const MAX_HEALTH_METRICS_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthAction {
    Report,
    Restart,
    Alert,
}

impl fmt::Display for HealthAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Report => "report",
            Self::Restart => "restart",
            Self::Alert => "alert",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthConfig {
    /// Optional loopback HTTP endpoint for a managed non-wire process.
    /// Changing it applies live on rescan; the process protocol changes only
    /// at its next spawn.
    pub http: Option<String>,
    pub cadence: Duration,
    pub deadline: Duration,
    pub failure_threshold: u32,
    pub on_degraded: HealthAction,
    pub on_failing: HealthAction,
    pub critical: bool,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            http: None,
            cadence: DEFAULT_HEALTH_CADENCE,
            deadline: DEFAULT_HEALTH_DEADLINE,
            failure_threshold: DEFAULT_HEALTH_FAILURE_THRESHOLD,
            on_degraded: HealthAction::Report,
            on_failing: HealthAction::Report,
            critical: false,
        }
    }
}

/// The supervisor's view of one module's health, relayed to clients over
/// channel-0 and rendered by `ck health`.
///
/// THIS TYPE IS WHERE THE ABSENCE MEANINGS ARE CREATED, which is why they are
/// stated here rather than only at the wire type a consumer reads. A reader can
/// look up what `None` means; only a writer can silently change it, and the
/// writer has no reason to go looking at a downstream contract before editing.
///
/// `last_probe_ms: None` MEANS NEVER PROBED, not probed-long-ago. It is cleared
/// back to `None` on re-registration precisely so a respawned module does not
/// carry its predecessor's timestamp — so an old value and an absent one call for
/// opposite readings, and anything that defaulted this to a number would make a
/// never-probed module indistinguishable from one probed at the epoch.
///
/// `detail` and `metrics` are `None` when the module published none on this
/// probe, which does not mean it reported nothing wrong — it is also the shape
/// when the probe never reached it. `last_probe_ms` is what separates those.
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleHealthStatus {
    pub status: SupervisorHealthStatus,
    pub last_probe_ms: Option<u64>,
    pub detail: Option<String>,
    pub metrics: Option<Value>,
    pub consecutive_failures: u32,
    /// Number of replies received after a recurring health probe's deadline.
    /// Unlike a timeout, every increment proves the module was alive.
    pub late_answer_count: u64,
    /// End-to-end latency of the newest late reply, measured from probe start.
    pub last_late_answer_latency_ms: Option<u64>,
    pub last_action: Option<String>,
    /// Set together with `last_action`; the pair moves as one, and both being
    /// absent means no escalation has ever been taken rather than that the last
    /// one succeeded.
    pub last_action_ms: Option<u64>,
}

impl Default for ModuleHealthStatus {
    fn default() -> Self {
        Self {
            status: SupervisorHealthStatus::Unknown,
            last_probe_ms: None,
            detail: None,
            metrics: None,
            consecutive_failures: 0,
            late_answer_count: 0,
            last_late_answer_latency_ms: None,
            last_action: None,
            last_action_ms: None,
        }
    }
}

impl SupervisorHandle {
    pub(crate) fn record_late_health_answer(
        &self,
        module_id: &str,
        latency_ms: u64,
    ) -> Result<bool, SuperviseError> {
        let Some(module) = self.get(module_id) else {
            return Ok(false);
        };
        self.host_health.record_late_module(module_id);
        update_snapshot(&module.inner.snapshot, Some(module_id), |state| {
            state.health.late_answer_count = state.health.late_answer_count.saturating_add(1);
            state.health.last_late_answer_latency_ms = Some(latency_ms);
            // A late answer is an answer: the module served the probe, just past
            // the deadline. Leaving the miss streak in place while logging
            // "proves the module is alive" is how a CPU-starved module that
            // answers every probe a few seconds late still marches to the
            // threshold and gets killed — the exact kill class `NoAnswer` is
            // excluded from `is_proof_of_death` to prevent. Slow-but-answering
            // is degradation, and degradation reports; it does not restart.
            state.health.consecutive_failures = 0;
        })?;
        Ok(true)
    }

    pub(crate) fn spawn_host_health_monitor(&self) -> JoinHandle<()> {
        self.host_health.spawn_monitor()
    }
}

impl SupervisedModule {
    /// Test-only: put one probe miss on the streak, the way
    /// `handle_health_probe_failure` does, so tests can assert what a later
    /// event does to the streak without driving the whole probe loop.
    #[cfg(test)]
    pub(crate) fn record_health_probe_failure_for_test(
        &self,
        detail: &str,
    ) -> Result<(), SuperviseError> {
        update_snapshot(&self.inner.snapshot, Some(&self.inner.module_id), |state| {
            state.health.consecutive_failures = state.health.consecutive_failures.saturating_add(1);
            state.health.detail = Some(detail.to_string());
        })
    }
}

#[derive(Debug, Default)]
pub(super) struct HealthProbeRuntime {
    spawn_generation: Option<u64>,
    configured_health: Option<HealthConfig>,
    pub(super) registered_connection: Option<crate::ConnectionId>,
    pub(super) advertised: bool,
    pub(super) next_probe_at: Option<Instant>,
    probe_index: u64,
}

pub(super) fn running_protocol(spec: &ModuleSpec, snapshot: &SharedSnapshot) -> ModuleProtocol {
    lock_snapshot(snapshot)
        .ok()
        .and_then(|state| state.spawned_protocol)
        .unwrap_or(spec.protocol)
}

impl HealthProbeRuntime {
    pub(super) fn refresh_registration(
        &mut self,
        spec: &ModuleSpec,
        runtime: &SupervisorRuntimeConfig,
        registry: &Registry,
        snapshot: &SharedSnapshot,
    ) {
        let generation = lock_snapshot(snapshot)
            .ok()
            .map(|state| state.spawn_generation);
        if self.spawn_generation != generation {
            // A replacement cannot reuse a deadline that is already due for its
            // predecessor. HTTP children get a full cadence to start; wire
            // children still wait for their own health-advertising HELLO.
            *self = Self {
                spawn_generation: generation,
                ..Self::default()
            };
        }
        if self.configured_health.as_ref() != Some(&runtime.health) {
            self.configured_health = Some(runtime.health.clone());
            self.next_probe_at = None;
            self.registered_connection = None;
            self.probe_index = 0;
        }
        // A non-wire process never registers. Only an explicitly configured
        // HTTP endpoint can arm its health probe; an absent HELLO is not a
        // health failure for that kind of process.
        if running_protocol(spec, snapshot) == ModuleProtocol::None {
            self.registered_connection = None;
            self.advertised = runtime.health.http.is_some();
            if !self.advertised {
                self.next_probe_at = None;
                let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                    let unknown = ModuleHealthStatus::default();
                    if state.health != unknown {
                        state.health = unknown;
                        state.last_probe_evidence = None;
                    }
                });
            } else if self.next_probe_at.is_none() {
                self.next_probe_at = Some(
                    Instant::now()
                        + jittered_health_delay(&spec.module_id, 0, runtime.health.cadence),
                );
            }
            return;
        }

        let registration = match registry.get_module(&spec.module_id) {
            Ok(registration) => registration,
            Err(err) => {
                warn!(module_id = %spec.module_id, error = %err, "health prober could not read registry");
                self.advertised = false;
                self.next_probe_at = None;
                return;
            }
        };

        let Some(registration) = registration else {
            self.registered_connection = None;
            self.advertised = false;
            self.next_probe_at = None;
            return;
        };

        let advertised = registration
            .control_ops
            .iter()
            .any(|op| op == MODULE_CONTROL_OP_HEALTH_CHECK);
        if !advertised {
            self.registered_connection = Some(registration.connection_id);
            self.advertised = false;
            self.next_probe_at = None;
            let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                if state.health.status != SupervisorHealthStatus::Unknown
                    || state.health.consecutive_failures != 0
                    || state.health.last_probe_ms.is_some()
                    || state.health.detail.is_some()
                    || state.health.metrics.is_some()
                {
                    state.health.status = SupervisorHealthStatus::Unknown;
                    state.last_probe_evidence = None;
                    state.health.consecutive_failures = 0;
                    state.health.last_probe_ms = None;
                    state.health.detail = None;
                    state.health.metrics = None;
                }
            });
            return;
        }

        let reregistered = self.registered_connection != Some(registration.connection_id);
        self.registered_connection = Some(registration.connection_id);
        self.advertised = true;
        if reregistered || self.next_probe_at.is_none() {
            self.probe_index = 0;
            self.next_probe_at = Some(
                Instant::now() + jittered_health_delay(&spec.module_id, 0, runtime.health.cadence),
            );
            let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                state.health.status = SupervisorHealthStatus::Unknown;
                state.last_probe_evidence = None;
                state.health.consecutive_failures = 0;
                state.health.detail = None;
                state.health.metrics = None;
            });
        }
    }

    pub(super) fn wake_at(&self) -> Option<Instant> {
        if !self.advertised {
            return None;
        }
        self.next_probe_at
    }

    pub(super) fn due(&self) -> bool {
        self.advertised
            && self
                .next_probe_at
                .is_some_and(|next| Instant::now() >= next)
    }

    pub(super) fn schedule_next(&mut self, spec: &ModuleSpec, cadence: Duration) {
        self.probe_index = self.probe_index.wrapping_add(1);
        self.next_probe_at = Some(
            Instant::now() + jittered_health_delay(&spec.module_id, self.probe_index, cadence),
        );
    }
}

/// Evidence is separate from diagnostic prose so host starvation can hold only
/// silence-based restarts. A missing lane is still actionable during overload;
/// a timeout can instead mean the host has not scheduled a healthy module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HealthProbeEvidence {
    /// The module's control lane is gone. Proof of death.
    LaneDead,
    /// No reply within the deadline. Proves nothing about the module's state.
    NoAnswer,
    /// The module replied, but not with a usable health report. Proves it is alive.
    BadAnswer,
    /// The daemon could not ask. Says nothing about the module.
    Misconfigured,
}

#[derive(Debug)]
pub(super) struct HealthProbeError {
    pub(super) evidence: HealthProbeEvidence,
    message: String,
}

impl HealthProbeError {
    pub(super) fn lane_dead(message: impl Into<String>) -> Self {
        Self::with(HealthProbeEvidence::LaneDead, message)
    }

    pub(super) fn no_answer(message: impl Into<String>) -> Self {
        Self::with(HealthProbeEvidence::NoAnswer, message)
    }

    pub(super) fn bad_answer(message: impl Into<String>) -> Self {
        Self::with(HealthProbeEvidence::BadAnswer, message)
    }

    pub(super) fn misconfigured(message: impl Into<String>) -> Self {
        Self::with(HealthProbeEvidence::Misconfigured, message)
    }

    fn with(evidence: HealthProbeEvidence, message: impl Into<String>) -> Self {
        Self {
            evidence,
            message: message.into(),
        }
    }

    /// Whether this observation is proof the module cannot serve.
    ///
    /// Only `LaneDead` qualifies. `NoAnswer` is deliberately excluded: it is the
    /// variant that fires under CPU starvation, and treating it as proof is the
    /// defect this enum exists to make impossible to reintroduce silently.
    ///
    /// Outside a host-overload hold, silence still reaches the normal failure
    /// threshold so an isolated, genuinely wedged module can recover.
    #[allow(dead_code)]
    pub(super) fn is_proof_of_death(&self) -> bool {
        matches!(self.evidence, HealthProbeEvidence::LaneDead)
    }

    /// Short stable label for logs and the health snapshot.
    ///
    /// An operator reading `ck health` currently cannot tell "the module is gone"
    /// from "the module did not answer in five seconds", because both render as
    /// prose in the same field. These labels are what make the two
    /// distinguishable at a glance, and they are what a later restart-policy
    /// change will be argued from.
    pub(super) fn label(&self) -> &'static str {
        match self.evidence {
            HealthProbeEvidence::LaneDead => "lane-dead",
            HealthProbeEvidence::NoAnswer => "no-answer",
            HealthProbeEvidence::BadAnswer => "bad-answer",
            HealthProbeEvidence::Misconfigured => "daemon-misconfigured",
        }
    }
}

impl fmt::Display for HealthProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

pub(super) async fn run_health_probe_cycle(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<SupervisedChild>,
) {
    let now_ms = unix_ms_now();
    let delivery_probe = match &runtime.config_delivery {
        Some(delivery) => {
            delivery
                .health_probe(&spec.module_id, runtime.forwarding.as_deref(), registry)
                .await
        }
        None => None,
    };
    let http = (running_protocol(spec, snapshot) == ModuleProtocol::None)
        .then_some(runtime.health.http.as_deref())
        .flatten();
    let result = match http {
        Some(url) => probe_http_health(url, runtime.health.deadline)
            .await
            .map(|report| (report, None)),
        None => probe_module_health_from(&spec.module_id, runtime, None)
            .await
            .map(|(report, probed)| (report, Some(probed))),
    };
    match result {
        Ok((report, probed)) => {
            handle_health_report(
                spec,
                runtime,
                registry,
                process_liveness,
                snapshot,
                child,
                report,
                now_ms,
                delivery_probe,
                probed,
            )
            .await;
        }
        Err(err) => {
            if http.is_some() {
                let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                    state.health.status = SupervisorHealthStatus::Failing;
                });
            }
            handle_health_probe_failure(
                spec,
                runtime,
                registry,
                process_liveness,
                snapshot,
                child,
                err,
                now_ms,
                delivery_probe,
            )
            .await;
        }
    }
}

pub(crate) struct HttpProbeTarget<'a> {
    address: std::net::SocketAddr,
    localhost: bool,
    authority: &'a str,
    path: String,
}

/// Resolve only literal loopback endpoints, without DNS, redirects, proxies,
/// or TLS. A URL cannot turn a local health check into an outbound connection.
pub(crate) fn parse_http_probe_url(url: &str) -> Result<HttpProbeTarget<'_>, String> {
    if url.bytes().any(|b| b <= b' ' || b == 127) || url.contains('#') {
        return Err("must not contain whitespace, controls, or a fragment".into());
    }
    let rest = url
        .strip_prefix("http://")
        .ok_or("must use plain http://")?;
    let split = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, suffix) = rest.split_at(split);
    let (host, port) = if let Some(rest) = authority.strip_prefix("[::1]") {
        ("::1", rest)
    } else {
        let split = authority.find(':').unwrap_or(authority.len());
        authority.split_at(split)
    };
    let ip: std::net::IpAddr = match host {
        "127.0.0.1" | "localhost" => std::net::Ipv4Addr::LOCALHOST.into(),
        "::1" => std::net::Ipv6Addr::LOCALHOST.into(),
        _ => return Err("host must be 127.0.0.1, [::1], or localhost".into()),
    };
    let port = if port.is_empty() {
        80
    } else {
        port.strip_prefix(':')
            .and_then(|p| p.parse::<u16>().ok())
            .filter(|p| *p > 0)
            .ok_or("must have a valid nonzero TCP port")?
    };
    let path = if suffix.is_empty() {
        "/".into()
    } else if suffix.starts_with('?') {
        format!("/{suffix}")
    } else {
        suffix.into()
    };
    Ok(HttpProbeTarget {
        address: std::net::SocketAddr::new(ip, port),
        localhost: host == "localhost",
        authority,
        path,
    })
}

pub(super) async fn probe_http_health(
    url: &str,
    deadline: Duration,
) -> Result<HealthReport, HealthProbeError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
    let target = parse_http_probe_url(url).map_err(HealthProbeError::misconfigured)?;
    // Keep partial diagnostics outside the timed future so cancellation does
    // not discard a status line or body bytes already received.
    let mut response_status = String::new();
    let mut body = Vec::new();
    let probe = async {
        // Resolve localhost ourselves so a hosts-file override cannot turn
        // this into an outbound request, while IPv6-only local servers work.
        let connection = match tokio::net::TcpStream::connect(target.address).await {
            Err(_) if target.localhost => {
                tokio::net::TcpStream::connect((
                    std::net::Ipv6Addr::LOCALHOST,
                    target.address.port(),
                ))
                .await
            }
            result => result,
        };
        let mut stream = connection.map_err(|error| {
            HealthProbeError::no_answer(format!("HTTP connect failed: {error}"))
        })?;
        stream.write_all(format!("GET {} HTTP/1.1\r\nHost: {}\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n", target.path, target.authority).as_bytes()).await
            .map_err(|error| HealthProbeError::no_answer(format!("HTTP write failed: {error}")))?;
        let mut reader = BufReader::new(stream);
        let mut budget = 16 * 1024;
        let status = http_line(&mut reader, &mut budget).await?;
        let mut words = status.split_ascii_whitespace();
        let version = words.next();
        let code = words
            .next()
            .filter(|word| word.len() == 3)
            .and_then(|word| word.parse::<u16>().ok());
        if !matches!(version, Some("HTTP/1.1" | "HTTP/1.0"))
            || !code.is_some_and(|code| (100..600).contains(&code))
        {
            return Err(HealthProbeError::bad_answer(format!(
                "invalid HTTP status: {status}"
            )));
        }
        let code = code.expect("validated status code");
        response_status = status.clone();
        let mut length = None;
        let mut chunked = false;
        loop {
            let line = http_line(&mut reader, &mut budget).await?;
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = Some(value.trim().parse::<u64>().map_err(|_| {
                        HealthProbeError::bad_answer("invalid HTTP Content-Length")
                    })?);
                } else if name.eq_ignore_ascii_case("transfer-encoding") {
                    chunked = value.trim().eq_ignore_ascii_case("chunked");
                }
            }
        }
        if chunked {
            while body.len() < 200 {
                let line = http_line(&mut reader, &mut budget).await?;
                let size = u64::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
                    .map_err(|_| HealthProbeError::bad_answer("invalid HTTP chunk size"))?;
                if size == 0 {
                    break;
                }
                let count = size.min((200 - body.len()) as u64) as usize;
                let start = body.len();
                (&mut reader)
                    .take(count as u64)
                    .read_to_end(&mut body)
                    .await
                    .map_err(|error| {
                        HealthProbeError::no_answer(format!("HTTP body read failed: {error}"))
                    })?;
                if body.len() - start != count {
                    return Err(HealthProbeError::bad_answer("truncated HTTP chunk"));
                }
                if size > count as u64 || body.len() == 200 {
                    break;
                }
                if !http_line(&mut reader, &mut budget).await?.is_empty() {
                    return Err(HealthProbeError::bad_answer("invalid HTTP chunk delimiter"));
                }
            }
        } else {
            reader
                .take(length.unwrap_or(200).min(200))
                .read_to_end(&mut body)
                .await
                .map_err(|error| {
                    HealthProbeError::no_answer(format!("HTTP body read failed: {error}"))
                })?;
        }
        if (200..300).contains(&code) {
            Ok(HealthReport::ok())
        } else {
            Err(HealthProbeError::bad_answer(
                "HTTP health endpoint returned non-2xx",
            ))
        }
    };
    let mut result = timeout(deadline, probe).await.unwrap_or_else(|_| {
        Err(HealthProbeError::no_answer(format!(
            "HTTP probe timed out after {deadline:?}"
        )))
    });
    if let Err(error) = &mut result {
        if !response_status.is_empty() {
            error.message = format!(
                "{}; {response_status}: {}",
                error.message,
                String::from_utf8_lossy(&body)
            );
        }
    }
    result
}

async fn http_line(
    reader: &mut tokio::io::BufReader<tokio::net::TcpStream>,
    remaining: &mut usize,
) -> Result<String, HealthProbeError> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    let mut line = Vec::new();
    (&mut *reader)
        .take(*remaining as u64)
        .read_until(b'\n', &mut line)
        .await
        .map_err(|error| {
            HealthProbeError::no_answer(format!("HTTP header read failed: {error}"))
        })?;
    *remaining -= line.len();
    if !line.ends_with(b"\r\n") {
        return Err(HealthProbeError::bad_answer(
            "HTTP headers are incomplete or exceed 16 KiB",
        ));
    }
    line.truncate(line.len() - 2);
    String::from_utf8(line).map_err(|_| HealthProbeError::bad_answer("HTTP header is not UTF-8"))
}

pub(super) async fn probe_module_health(
    module_id: &str,
    runtime: &SupervisorRuntimeConfig,
    drain_deadline: Option<Instant>,
) -> Result<HealthReport, HealthProbeError> {
    probe_module_health_from(module_id, runtime, drain_deadline)
        .await
        .map(|(report, _)| report)
}

/// [`probe_module_health`], also returning the connection whose registration
/// answered, so its answer is credited to that registration and no other.
async fn probe_module_health_from(
    module_id: &str,
    runtime: &SupervisorRuntimeConfig,
    drain_deadline: Option<Instant>,
) -> Result<(HealthReport, ConnectionId), HealthProbeError> {
    let Some(forwarding) = runtime.forwarding.as_ref() else {
        return Err(HealthProbeError::misconfigured(
            "supervisor was not configured with a forwarding table",
        ));
    };
    let probe_started_at = Instant::now();
    let mut deadline = probe_started_at + runtime.health.deadline;
    if let Some(drain_deadline) = drain_deadline {
        deadline = deadline.min(drain_deadline);
    }
    let pending = if drain_deadline.is_some() {
        forwarding.begin_drain_health_probe_rpc_for(
            module_id,
            MODULE_CONTROL_OP_HEALTH_CHECK,
            probe_started_at,
            deadline,
        )
    } else {
        forwarding.begin_health_probe_rpc_for(
            module_id,
            MODULE_CONTROL_OP_HEALTH_CHECK,
            probe_started_at,
            deadline,
        )
    }
    .map_err(|err| {
        // The endpoint is not registered, so there is no live control lane to
        // ask. That is the module being absent, not slow.
        HealthProbeError::lane_dead(format!("failed to begin health.check RPC: {err}"))
    })?;
    let probed = pending.endpoint.connection_id;
    await_health_probe(forwarding, pending, deadline, runtime.health.deadline)
        .await
        .map(|report| (report, probed))
}

/// [`probe_module_health`] for one endpoint rather than the id's active one.
///
/// A swap probes two processes that no by-id lookup reaches: its candidate
/// before cutover, and its superseded incumbent (for busy gauges) while the
/// incumbent drains. `deadline_cap` bounds the probe the way a drain deadline
/// bounds the by-id drain probe.
pub(super) async fn probe_endpoint_health(
    endpoint: crate::ModuleEndpointId,
    runtime: &SupervisorRuntimeConfig,
    deadline_cap: Option<Instant>,
) -> Result<HealthReport, HealthProbeError> {
    let Some(forwarding) = runtime.forwarding.as_ref() else {
        return Err(HealthProbeError::misconfigured(
            "supervisor was not configured with a forwarding table",
        ));
    };
    let probe_started_at = Instant::now();
    let mut deadline = probe_started_at + runtime.health.deadline;
    if let Some(cap) = deadline_cap {
        deadline = deadline.min(cap);
    }
    let pending = forwarding
        .begin_endpoint_health_probe_rpc_for(
            endpoint,
            MODULE_CONTROL_OP_HEALTH_CHECK,
            probe_started_at,
            deadline,
        )
        .map_err(|err| {
            HealthProbeError::lane_dead(format!("failed to begin health.check RPC: {err}"))
        })?;
    await_health_probe(forwarding, pending, deadline, runtime.health.deadline).await
}

/// Send a begun health probe and classify its answer.
async fn await_health_probe(
    forwarding: &ForwardingTable,
    pending: PendingModuleControlRpc,
    deadline: Instant,
    probe_budget: Duration,
) -> Result<HealthReport, HealthProbeError> {
    let PendingModuleControlRpc {
        endpoint,
        module_sink,
        negotiated_ver,
        corr,
        receiver,
    } = pending;
    let body = serde_json::to_vec(&ModuleControlRequest::HealthCheck {}).map_err(|err| {
        HealthProbeError::misconfigured(format!("failed to encode health.check: {err}"))
    })?;
    let frame = Frame::build_with_version(
        negotiated_ver,
        FrameType::Request,
        control_flags(),
        0,
        0,
        corr,
        body,
    )
    .map_err(|err| {
        HealthProbeError::misconfigured(format!("failed to build health.check frame: {err}"))
    })?;

    // The enqueue itself must be bounded by the probe deadline: FrameSink.send
    // blocks waiting for capacity when the module's egress queue is full, and an
    // unbounded await here freezes the whole supervision actor (it stops polling
    // Child::wait and supervisor commands), making the module unrecoverable
    // in-band. On timeout the probe fails like any transport failure.
    match timeout_at(deadline, module_sink.send(frame)).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            let _ = forwarding.cancel_module_control_rpc(endpoint, corr);
            // A closed sink means the module's egress channel is gone -- the
            // receiving half is dropped when its connection tears down. Proof.
            return Err(HealthProbeError::lane_dead(format!(
                "failed to send health.check: {err}"
            )));
        }
        Err(_elapsed) => {
            let _ = forwarding.cancel_module_control_rpc(endpoint, corr);
            // A full egress queue means the module is not draining its socket, which
            // is consistent with a wedged module AND with one whose reader is merely
            // starved. Silence, not proof.
            return Err(HealthProbeError::no_answer(
                "health.check send timed out before enqueue (module egress full)",
            ));
        }
    }

    match timeout_at(deadline, receiver).await {
        Ok(Ok(ModuleControlRpcOutcome::RawResponse(_))) => Err(HealthProbeError::bad_answer(
            "health.check returned an unexpected config response",
        )),
        // Each arm records WHAT WAS OBSERVED. Four of them are the module
        // demonstrably answering -- rejected, non-health, malformed, wrong op --
        // and those prove it is alive even though the probe failed.
        Ok(Ok(ModuleControlRpcOutcome::Response(response))) => {
            response.health_report().ok_or_else(|| {
                HealthProbeError::bad_answer("health.check RPC returned a non-health response")
            })
        }
        Ok(Ok(ModuleControlRpcOutcome::Rejected(body))) => Err(HealthProbeError::bad_answer(
            format!("health.check rejected: {}", body.message),
        )),
        Ok(Ok(ModuleControlRpcOutcome::ModuleGone(message))) => {
            Err(HealthProbeError::lane_dead(message))
        }
        Ok(Ok(ModuleControlRpcOutcome::MalformedResponse(message))) => {
            Err(HealthProbeError::bad_answer(message))
        }
        Ok(Ok(ModuleControlRpcOutcome::UnexpectedOp { expected, actual })) => {
            Err(HealthProbeError::bad_answer(format!(
                "expected module-control op '{expected}', got '{actual}'"
            )))
        }
        // A reply that crosses the deadline before this waiter observes it is
        // still proof of life. The forwarding path records its end-to-end latency
        // before delivering this classification.
        Ok(Ok(ModuleControlRpcOutcome::DeadlineElapsed)) => Err(HealthProbeError::bad_answer(
            "module answered health.check after its daemon deadline",
        )),
        Ok(Err(_)) => Err(HealthProbeError::misconfigured(
            "health.check waiter was canceled before the module responded",
        )),
        Err(_) => {
            let _ = forwarding.tombstone_health_probe_rpc(endpoint, corr);
            Err(HealthProbeError::no_answer(format!(
                "module did not answer health.check within {probe_budget:?}"
            )))
        }
    }
}

/// Act on a health probe's answer. `probed` is the connection whose
/// registration answered (`None` for an HTTP probe, which reaches no
/// registration); only that registration's spawn is credited with readiness.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_health_report(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<SupervisedChild>,
    report: HealthReport,
    now_ms: u64,
    delivery_probe: Option<crate::config_gate::runtime::DeliveryProbe>,
    probed: Option<ConnectionId>,
) {
    let delivery_failed = match (&runtime.config_delivery, delivery_probe) {
        (Some(delivery), Some(probe)) => {
            delivery
                .health_result(
                    &spec.module_id,
                    runtime.forwarding.as_deref(),
                    registry,
                    probe,
                    report.status == HealthStatus::Ok,
                )
                .await
        }
        _ => false,
    };
    if let (Some(delivery), Some(spawn)) = (
        &runtime.config_delivery,
        child
            .as_ref()
            .and_then(|child| child.config_delivery.as_ref()),
    ) {
        delivery
            .mark_readiness(
                &spec.module_id,
                runtime.forwarding.as_deref(),
                probed,
                spawn,
                report.status == HealthStatus::Ok,
            )
            .await;
    }
    let status = supervisor_health_status(report.status);
    let detail = report.detail.clone();
    let metrics = truncate_health_metrics(report.metrics);
    let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
        state.health.status = status;
        state.last_probe_evidence = None;
        state.health.last_probe_ms = Some(now_ms);
        state.health.detail = detail.clone();
        state.health.metrics = metrics.clone();
        state.health.consecutive_failures = 0;
        state.failed_retry.observe_health(
            report.status == HealthStatus::Ok,
            runtime.restart_policy.window,
        );
    });

    if delivery_failed {
        return;
    }
    let action = match report.status {
        HealthStatus::Ok => return,
        HealthStatus::Degraded => runtime.health.on_degraded,
        HealthStatus::Failing => runtime.health.on_failing,
    };
    apply_l3_health_action(
        spec,
        runtime,
        registry,
        process_liveness,
        snapshot,
        child,
        status,
        detail.as_deref(),
        action,
        now_ms,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_health_probe_failure(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<SupervisedChild>,
    err: HealthProbeError,
    now_ms: u64,
    delivery_probe: Option<crate::config_gate::runtime::DeliveryProbe>,
) {
    let delivery_failed = match (&runtime.config_delivery, delivery_probe) {
        (Some(delivery), Some(probe)) => {
            delivery
                .health_result(
                    &spec.module_id,
                    runtime.forwarding.as_deref(),
                    registry,
                    probe,
                    false,
                )
                .await
        }
        _ => false,
    };
    let threshold = runtime.health.failure_threshold.max(1);
    let held = if err.evidence == HealthProbeEvidence::NoAnswer {
        runtime.host_health.record_late_module(&spec.module_id)
    } else {
        false
    };
    let mut failures = 0;
    // Carry the evidence class into the operator-visible detail. Without it,
    // "module did not answer within 5s" and "the control lane is gone" are two
    // prose strings in the same field, and the reader has to know the codebase to
    // tell which one is proof of anything.
    let detail = if held {
        format!("[{}] {err}; host overloaded", err.label())
    } else {
        format!("[{}] {err}", err.label())
    };
    let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
        state.last_probe_evidence = Some(err.evidence);
        state
            .failed_retry
            .observe_health(false, runtime.restart_policy.window);
        // A failed wire probe invalidates the last report, even before the
        // restart threshold. HTTP probes already mark failures as Failing.
        if held {
            state.health.status = SupervisorHealthStatus::Degraded;
        } else if state.spawned_protocol.unwrap_or(spec.protocol) == ModuleProtocol::Subc {
            state.health.status = SupervisorHealthStatus::Unknown;
        }
        state.health.last_probe_ms = Some(now_ms);
        // Held silence breaks the streak: after the host recovers, only new
        // failures count. Replaying pre-overload misses would restart immediately.
        state.health.consecutive_failures = if held {
            0
        } else {
            state.health.consecutive_failures.saturating_add(1)
        };
        state.health.detail = Some(detail.clone());
        state.health.metrics = None;
        failures = state.health.consecutive_failures;
    });

    if delivery_failed {
        return;
    }
    if held || failures < threshold {
        warn!(
            module_id = %spec.module_id,
            consecutive_failures = failures,
            threshold,
            evidence = err.label(),
            detail = %detail,
            "health.check probe failed"
        );
        return;
    }

    let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
        state.state = ModuleState::Unresponsive;
        state.health.status = SupervisorHealthStatus::Unresponsive;
    });
    // The evidence class is logged at the kill site because this is the line an
    // operator reads after an unexplained restart. A streak of `no-answer` under
    // machine load is the known false-positive shape; a `lane-dead` is not.
    if runtime.health.critical {
        error!(
            module_id = %spec.module_id,
            status = "unresponsive",
            evidence = err.label(),
            detail = %detail,
            "critical module health alert"
        );
    } else {
        warn!(
            module_id = %spec.module_id,
            status = "unresponsive",
            evidence = err.label(),
            detail = %detail,
            "module health threshold breached"
        );
    }
    if let Err(err) = health_restart_child(
        spec,
        runtime,
        registry,
        process_liveness,
        snapshot,
        child,
        SupervisorHealthStatus::Unresponsive,
        Some(&detail),
        now_ms,
    )
    .await
    {
        error!(module_id = %spec.module_id, error = %err, "health-triggered restart failed");
    }
}

#[allow(clippy::too_many_arguments)]
async fn apply_l3_health_action(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<SupervisedChild>,
    status: SupervisorHealthStatus,
    detail: Option<&str>,
    action: HealthAction,
    now_ms: u64,
) {
    record_health_action(snapshot, &spec.module_id, action.to_string(), now_ms);
    match action {
        HealthAction::Report => {
            info!(
                module_id = %spec.module_id,
                status = ?status,
                detail,
                "module reported non-ok health"
            );
        }
        HealthAction::Alert => {
            error!(
                module_id = %spec.module_id,
                status = ?status,
                detail,
                "module health alert"
            );
        }
        HealthAction::Restart => {
            if let Err(err) = health_restart_child(
                spec,
                runtime,
                registry,
                process_liveness,
                snapshot,
                child,
                status,
                detail,
                now_ms,
            )
            .await
            {
                error!(module_id = %spec.module_id, error = %err, "health-triggered restart failed");
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn health_restart_child(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<SupervisedChild>,
    status: SupervisorHealthStatus,
    detail: Option<&str>,
    now_ms: u64,
) -> Result<(), SuperviseError> {
    budgeted_restart_child(
        spec,
        runtime,
        registry,
        process_liveness,
        snapshot,
        child,
        status,
        detail,
        now_ms,
        true,
    )
    .await
}

pub(super) fn record_health_action(
    snapshot: &SharedSnapshot,
    module_id: &str,
    action: String,
    now_ms: u64,
) {
    let _ = update_snapshot(snapshot, Some(module_id), |state| {
        state.health.last_action = Some(action);
        state.health.last_action_ms = Some(now_ms);
    });
}

fn supervisor_health_status(status: HealthStatus) -> SupervisorHealthStatus {
    match status {
        HealthStatus::Ok => SupervisorHealthStatus::Ok,
        HealthStatus::Degraded => SupervisorHealthStatus::Degraded,
        HealthStatus::Failing => SupervisorHealthStatus::Failing,
    }
}

/// Caps the metrics blob stored in the cached supervisor snapshot, which is
/// returned to every `supervisor.list` and `supervisor.health` caller.
///
/// This cap is deliberately NOT applied on the one-shot `supervisor.health_probe`
/// path: that request exists to return a module's complete metrics object, and
/// `ck health <module-id>` documents it as the way to see what the cached view
/// truncates. The asymmetry is the feature.
///
/// So a new caller must decide which side it is on rather than assume the cap is
/// universal. Reaching for it on a fresh-probe path would silently reintroduce
/// the truncation that path exists to avoid.
fn truncate_health_metrics(metrics: Option<Value>) -> Option<Value> {
    let metrics = metrics?;
    match serde_json::to_vec(&metrics) {
        Ok(encoded) if encoded.len() > MAX_HEALTH_METRICS_BYTES => Some(serde_json::json!({
            "truncated": true,
            "original_bytes": encoded.len(),
        })),
        Ok(_) | Err(_) => Some(metrics),
    }
}

/// Spread health probes so a fleet-wide restart does not converge them.
///
/// The delay is derived from the module id and probe index rather than a random
/// source, so it is deterministic per module: a module keeps its own offset
/// across daemon restarts instead of re-rolling into a collision.
pub(super) fn jittered_health_delay(
    module_id: &str,
    probe_index: u64,
    cadence: Duration,
) -> Duration {
    if cadence.is_zero() {
        return Duration::ZERO;
    }
    let cadence_ms = cadence.as_millis() as u64;
    // This early return is REDUNDANT, deliberately, and a mutation run will show
    // it surviving removal. Recording why here so the next person to notice does
    // not have to re-derive it:
    //
    // - It is unreachable in practice. `positive_millis` in daemon_config rejects
    //   a zero cadence and builds the Duration from whole milliseconds, so a
    //   sub-millisecond cadence cannot come from config.
    // - Even if reached it changes no answer. The `.max(1)` below makes the span
    //   1, and `hash % 1` is 0, so the fall-through returns `cadence` unchanged
    //   -- exactly what this returns.
    //
    // Kept as a guard against a future widening of the config parser (accepting
    // microseconds, say), which would make the sub-millisecond case reachable.
    // The `.max(1)` is the load-bearing half TODAY: remove it and the modulo
    // divides by zero. Remove this and nothing changes.
    if cadence_ms == 0 {
        return cadence;
    }
    // Note that this never returns less than one cadence, including for the FIRST
    // probe. So a freshly registered module reports health `unknown` for a full
    // cadence plus jitter -- 30-33s at the default -- no matter how quickly it is
    // ready to answer.
    //
    // That is a property of the supervisor's schedule, not of any module: an
    // operator watching a restart sees `unknown` and cannot tell it from a module
    // that is slow to warm. Measured on two unrelated modules, both flipping to
    // `ok` between 22s and 32s after restart.
    //
    // Left as-is because spreading the first probe is what keeps a fleet-wide
    // restart from firing fourteen simultaneous probes into a cold machine. The
    // alternative -- probe at t+0 and jitter only from the second onward -- trades
    // that thundering herd for a faster first reading.
    let jitter_span = (cadence_ms / 10).max(1);
    let hash = module_id.as_bytes().iter().fold(
        probe_index.wrapping_mul(0x9E37_79B9_7F4A_7C15),
        |acc, byte| {
            acc.wrapping_mul(1099511628211)
                .wrapping_add(u64::from(*byte))
        },
    );
    cadence + Duration::from_millis(hash % jitter_span)
}
