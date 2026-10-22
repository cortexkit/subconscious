//! Pure mappings from what the daemon and modules reply to what the screens
//! show. Nothing here does I/O or reads a clock: the functions take their
//! inputs, the time included, as arguments, so each mapping can be tested
//! against recorded replies.
//!
//! The rules they follow:
//! - A value no reply vouches for is `None` (drawn "unknown"), never zero.
//! - A failing source keeps its error text; its numbers aren't invented.
//! - Module wording (health detail, metric keys) is passed through as the
//!   module wrote it.

use std::collections::HashMap;

use serde_json::Value;
use subc_control::{
    ActivityEntry as WireActivity, ChildResourceUsage, LaunchSpecAgreement, PendingReloadVerdict,
    ReloadPathAgreement, RunningImageAgreement, StderrTailEntry, SupervisorEntry,
    SupervisorHealthEntry, SupervisorHealthStatus, SupervisorModuleProvenance,
};
use subc_protocol::Principal;

use crate::metrics::{all_rows, pane_rows};
use crate::model::{
    Account, ActivityEntry, ActivityKey, ActivityRow, ActivityRowKind, Agent, Agents, Backups,
    BuildServer, Device, Devices, Fault, LogLevel, LogLine, Metric, ProcessEvent, ProcessEventKind,
    Provider, Status, UsageWindow,
};
use crate::quota;
use crate::time::{ago, Timestamp, UtcOffset};

// ---- modules -------------------------------------------------------------------

/// A module's status from the supervisor's state and its cached health.
pub fn module_status(state: &str, health: SupervisorHealthStatus) -> Status {
    match state {
        "running" => match health {
            SupervisorHealthStatus::Ok => Status::Ok,
            SupervisorHealthStatus::Degraded => Status::Degraded,
            SupervisorHealthStatus::Failing | SupervisorHealthStatus::Unresponsive => {
                Status::Failed
            }
            SupervisorHealthStatus::Unknown => Status::Unknown,
        },
        "starting" | "restarting" | "draining" => Status::Starting,
        "unresponsive" | "failed" => Status::Failed,
        "stopped" | "disabled" => Status::Stopped,
        _ => Status::Unknown,
    }
}

/// Why a restart is waiting, from the daemon's comparison of what is
/// configured with what is running; `None` when they agree or it can't say.
pub fn pending_restart(verdict: Option<&PendingReloadVerdict>) -> Option<String> {
    let v = verdict?;
    if matches!(v.path, ReloadPathAgreement::Mismatch { .. }) {
        return Some("A different program is configured; it runs after a restart".into());
    }
    if matches!(v.image, RunningImageAgreement::Mismatch { .. }) {
        return Some("A new build is on disk; it runs after a restart".into());
    }
    if matches!(v.spec, Some(LaunchSpecAgreement::Mismatch)) {
        return Some("New settings wait for a restart".into());
    }
    None
}

/// The build a module declares: the first eight characters of its commit,
/// or "unknown".
pub fn module_build(provenance: Option<&SupervisorModuleProvenance>) -> (String, String) {
    use subc_control::ModuleDeclaredProvenance;
    let sha = provenance.and_then(|p| match &p.module_declared {
        ModuleDeclaredProvenance::Reported { build } => build.build_git_sha.clone(),
        _ => None,
    });
    match sha.filter(|s| !s.is_empty() && s != "unavailable") {
        Some(sha) => (sha.chars().take(8).collect(), sha),
        None => ("unknown".into(), "unknown".into()),
    }
}

/// A module's cumulative CPU milliseconds and memory bytes from its list
/// entry, when the daemon measured them.
pub fn resource_reading(entry: &SupervisorEntry) -> Option<(u64, u64)> {
    match &entry.resources {
        Some(ChildResourceUsage::Measured(r)) => Some((
            r.cpu_user_ms.saturating_add(r.cpu_system_ms),
            r.memory_bytes,
        )),
        _ => None,
    }
}

/// The module's health detail and headline metrics, in its own words.
pub fn health_view(
    health: Option<&SupervisorHealthEntry>,
    offset: UtcOffset,
) -> (String, Vec<Metric>) {
    match health {
        Some(h) => detail_and_metrics(h.detail.as_deref(), h.metrics.as_ref(), offset),
        None => (String::new(), Vec::new()),
    }
}

/// The daemon's cached health view replaces a metrics object over its size
/// cap with `{"truncated": true, "original_bytes": N}`. That marker is the
/// daemon's, not the module's, so it is never shown as the module's metrics.
pub fn is_truncation_marker(metrics: &Value) -> bool {
    metrics.as_object().is_some_and(|m| {
        m.get("truncated") == Some(&Value::Bool(true))
            && m.contains_key("original_bytes")
            && m.len() == 2
    })
}

/// Maps a health detail and metrics object to the summary line and the
/// headline metric rows the Modules pane shows.
pub fn detail_and_metrics(
    detail: Option<&str>,
    metrics: Option<&Value>,
    offset: UtcOffset,
) -> (String, Vec<Metric>) {
    let summary = detail.unwrap_or_default().to_string();
    let metrics = metrics
        .filter(|m| !is_truncation_marker(m))
        .map(|m| pane_rows(m, offset))
        .unwrap_or_default()
        .into_iter()
        .map(|(label, value)| Metric { label, value })
        .collect();
    (summary, metrics)
}

/// Every metric in a metrics object, by full key, for the all-metrics list;
/// nothing for the cache's size marker.
pub fn every_metric(metrics: Option<&Value>, offset: UtcOffset) -> Vec<Metric> {
    metrics
        .filter(|m| !is_truncation_marker(m))
        .map(|m| all_rows(m, offset))
        .unwrap_or_default()
        .into_iter()
        .map(|(label, value)| Metric { label, value })
        .collect()
}

// ---- CPU -----------------------------------------------------------------------

/// CPU percent between two readings of the same process: CPU time used over
/// wall time passed. `None` when no wall time passed or the counter went
/// backwards (which only a different process can do).
pub fn cpu_percent(prev: (i64, u64), next: (i64, u64)) -> Option<f64> {
    let wall = next.0 - prev.0;
    if wall <= 0 || next.1 < prev.1 {
        return None;
    }
    Some((next.1 - prev.1) as f64 / wall as f64 * 100.0)
}

/// How long ck keeps CPU and memory samples, for the sparklines.
pub const HISTORY_MS: i64 = 10 * 60 * 1000;

#[derive(Clone, Debug, Default)]
struct Series {
    pid: Option<u32>,
    /// The last reading: wall-clock ms and cumulative CPU ms.
    last: Option<(i64, u64)>,
    cpu: Vec<(i64, f64)>,
    mem: Vec<(i64, f64)>,
}

/// Per-module CPU and memory series built from successive readings. A
/// reading from a different pid than the last starts a fresh series,
/// because CPU time is counted per process and a difference across two
/// processes means nothing.
#[derive(Clone, Debug, Default)]
pub struct CpuTracker {
    series: HashMap<String, Series>,
}

impl CpuTracker {
    pub fn record(
        &mut self,
        module: &str,
        pid: Option<u32>,
        at_ms: i64,
        cpu_ms: u64,
        mem_bytes: u64,
    ) {
        let s = self.series.entry(module.to_string()).or_default();
        if s.pid != pid {
            *s = Series {
                pid,
                ..Series::default()
            };
        }
        if let Some(prev) = s.last {
            if let Some(pct) = cpu_percent(prev, (at_ms, cpu_ms)) {
                s.cpu.push((at_ms, pct));
            }
        }
        s.last = Some((at_ms, cpu_ms));
        s.mem.push((at_ms, mem_bytes as f64 / (1024.0 * 1024.0)));
        let cutoff = at_ms - HISTORY_MS;
        s.cpu.retain(|(t, _)| *t >= cutoff);
        s.mem.retain(|(t, _)| *t >= cutoff);
    }

    /// Forgets a module's series, for a process that is gone.
    pub fn forget(&mut self, module: &str) {
        self.series.remove(module);
    }

    /// The latest CPU percent and memory in MB of the module's current
    /// process.
    pub fn current(&self, module: &str) -> (Option<f64>, Option<f64>) {
        match self.series.get(module) {
            Some(s) => (s.cpu.last().map(|(_, v)| *v), s.mem.last().map(|(_, v)| *v)),
            None => (None, None),
        }
    }

    /// The CPU and memory samples, oldest first.
    pub fn history(&self, module: &str) -> (Vec<f64>, Vec<f64>) {
        match self.series.get(module) {
            Some(s) => (
                s.cpu.iter().map(|(_, v)| *v).collect(),
                s.mem.iter().map(|(_, v)| *v).collect(),
            ),
            None => (Vec::new(), Vec::new()),
        }
    }
}

// ---- module replies ------------------------------------------------------------

/// A module reply's payload: entorhinal wraps its replies in `result`.
pub fn unwrap_result(v: &Value) -> &Value {
    v.get("result").unwrap_or(v)
}

/// An error a module reported inside an otherwise successful reply: its
/// `code` when it gives one (a machine code), else its `message` (a
/// sentence), else the error value itself as text (untyped).
pub fn reply_error(v: &Value) -> Option<Fault> {
    let e = v.get("error")?;
    if e.is_null() {
        return None;
    }
    if let Some(code) = e.get("code").and_then(Value::as_str) {
        return Some(Fault::code(code));
    }
    if let Some(message) = e.get("message").and_then(Value::as_str) {
        return Some(Fault::sentence(message));
    }
    Some(Fault::from(
        e.as_str().map_or_else(|| e.to_string(), str::to_string),
    ))
}

/// Looks a dotted path up in a metrics object, accepting both nesting
/// (`{"a": {"b": 1}}`) and a flat dotted key (`{"a.b": 1}`).
pub fn metric<'a>(metrics: &'a Value, path: &str) -> Option<&'a Value> {
    if let Some(v) = metrics.get(path) {
        return Some(v);
    }
    let mut cur = metrics;
    for part in path.split('.') {
        cur = cur.get(part)?;
    }
    Some(cur)
}

fn metric_u64(metrics: &Value, path: &str) -> Option<u64> {
    metric(metrics, path).and_then(Value::as_u64)
}

fn metric_f64(metrics: &Value, path: &str) -> Option<f64> {
    metric(metrics, path).and_then(Value::as_f64)
}

/// The build server from the motor module's health metrics. The history, when
/// the module sends one, is busy permits over capacity per minute; a missing
/// minute is a gap (NaN), not a zero. Without it, `own_samples` (ck's own
/// readings of the same ratio, in percent) are plotted instead.
pub fn build_server(metrics: Option<&Value>, own_samples: &[f64]) -> BuildServer {
    let Some(m) = metrics else {
        return BuildServer {
            busy: None,
            slots: None,
            queued: None,
            wait_secs: None,
            load_history: own_samples.to_vec(),
        };
    };
    let busy = metric_u64(m, "build_server.permits_held").map(|v| v as u32);
    let slots = metric_u64(m, "build_server.permits_cap").map(|v| v as u32);
    let queued = metric_u64(m, "build_server.queue_depth").map(|v| v as u32);
    let wait_secs = metric_f64(m, "queue_wait_1h.p90_ms")
        .or_else(|| metric_f64(m, "build_server.queue_wait_1h.p90_ms"))
        .map(|ms| ms / 1000.0);
    let history = metric(m, "history").or_else(|| metric(m, "build_server.history"));
    let load_history = match history {
        Some(Value::Array(points)) if !points.is_empty() => history_load(points),
        Some(compact @ Value::Object(_)) => {
            compact_history_load(compact).unwrap_or_else(|| own_samples.to_vec())
        }
        _ => own_samples.to_vec(),
    };
    BuildServer {
        busy,
        slots,
        queued,
        wait_secs,
        load_history,
    }
}

/// Minute points `{minute_ms, permits_held, permits_cap}`, oldest first, as
/// percent busy, with a NaN for each minute missing between two points (at
/// most 180 in a row, the length of the whole history).
fn history_load(points: &[Value]) -> Vec<f64> {
    let mut out = Vec::new();
    let mut prev_minute: Option<i64> = None;
    for p in points {
        let Some(minute) = p.get("minute_ms").and_then(Value::as_i64) else {
            continue;
        };
        if let Some(prev) = prev_minute {
            let missing = ((minute - prev) / 60_000 - 1).clamp(0, 180);
            out.extend(std::iter::repeat_n(f64::NAN, missing as usize));
        }
        prev_minute = Some(minute);
        let held = p.get("permits_held").and_then(Value::as_f64);
        let cap = p.get("permits_cap").and_then(Value::as_f64);
        out.push(match (held, cap) {
            (Some(h), Some(c)) if c > 0.0 => h / c * 100.0,
            _ => f64::NAN,
        });
    }
    out
}

/// Motor's compact history: `{start_minute_ms, step_ms, permits_held: [...],
/// permits_cap: N or [...], queue_depth: [...], queue_p90_ms: [...]}`, one
/// slot per step, oldest first, with `null` for a step without a sample. As
/// percent busy, a NaN for each empty step. `None` when `permits_held` isn't
/// a list or `permits_cap` is missing.
fn compact_history_load(h: &Value) -> Option<Vec<f64>> {
    let held = h.get("permits_held")?.as_array()?;
    let cap = h.get("permits_cap")?;
    let cap_at = |i: usize| -> Option<f64> {
        match cap {
            Value::Array(caps) => caps.get(i).and_then(Value::as_f64),
            other => other.as_f64(),
        }
    };
    Some(
        held.iter()
            .enumerate()
            .map(|(i, v)| match (v.as_f64(), cap_at(i)) {
                (Some(h), Some(c)) if c > 0.0 => h / c * 100.0,
                _ => f64::NAN,
            })
            .collect(),
    )
}

/// A Unix time given in seconds or milliseconds, or as RFC 3339 text.
fn instant(v: &Value) -> Option<Timestamp> {
    if let Some(n) = v.as_f64() {
        // Milliseconds since 1970 pass 1e11 in 1973; seconds won't until
        // the year 5138.
        let ms = if n >= 1e11 { n } else { n * 1000.0 };
        return Some(Timestamp::from_unix_millis(ms as i64));
    }
    v.as_str().and_then(parse_rfc3339)
}

/// Backups from the engram module's health. A non-null
/// `lastPublishFailure` means nothing is uploading, whatever
/// `publishing.state` says (the module clears it when a publish succeeds),
/// so "uploading now" shows only while a publish is in flight with no
/// failure on record. Otherwise the module's own health sentence shows.
pub fn backups(metrics: Option<&Value>, detail: Option<&str>) -> Backups {
    let Some(m) = metrics else {
        return Backups {
            last_upload_at: None,
            uploading: false,
            status: Status::Unknown,
            detail: detail.map(str::to_string),
        };
    };
    let last_upload_at = metric(m, "lastPublishUnix").and_then(instant);
    let in_flight = metric(m, "publishing.state").and_then(Value::as_str) == Some("in_flight");
    // Older builds don't send the field at all, which reads like null.
    let failing = metric(m, "lastPublishFailure").is_some_and(|f| !f.is_null());
    let status = match metric(m, "publishStaleness").and_then(Value::as_str) {
        Some("fresh") => Status::Ok,
        Some("behind") | Some("never_published") => Status::Degraded,
        Some("stale") => Status::Failed,
        _ => Status::Unknown,
    };
    Backups {
        last_upload_at,
        uploading: in_flight && !failing,
        status,
        detail: detail.map(str::to_string),
    }
}

/// How many models the fusiform module prices and when it last checked:
/// `None` for counts it hasn't made (null) or doesn't send (older builds).
pub fn models(
    metrics: Option<&Value>,
    probed_at: Option<Timestamp>,
) -> (Option<u64>, Option<Timestamp>) {
    let Some(m) = metrics else {
        return (None, None);
    };
    let priced = metric_u64(m, "models_priced");
    let checked = match (metric_f64(m, "observation_age_ms"), probed_at) {
        (Some(age), Some(at)) => Some(at.add_millis(-(age as i64))),
        _ => None,
    };
    (priced, checked)
}

// ---- usage ---------------------------------------------------------------------

/// Who an account is: its email, else its account label (a UUID cut to its
/// first eight characters, as `ck quota` prints it).
fn account_who(entry: &Value) -> String {
    let email = entry
        .get("accountInfo")
        .and_then(|i| i.get("email"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    match email {
        Some(e) => e.to_string(),
        None => {
            let label = quota::table_account_label(entry);
            if label.is_empty() {
                "account".into()
            } else {
                label
            }
        }
    }
}

/// Why an account's row can't be read, or `Err(())` for a disconnected row
/// the quota module classes as nothing to fix (no credential configured, or
/// no quota on the account), which isn't listed at all.
fn account_failure(entry: &Value) -> Result<Option<Fault>, ()> {
    if quota::quota_entry_is_connected(entry) {
        return Ok(None);
    }
    match quota::quota_disconnect_kind(entry) {
        quota::QuotaDisconnectKind::Inert => Err(()),
        // `errorClass` is the producer's machine code; `error` is its
        // sentence for people.
        _ => Ok(Some(
            entry
                .get("errorClass")
                .and_then(Value::as_str)
                .map(Fault::code)
                .or_else(|| quota::entry_error_detail(entry).map(Fault::sentence))
                .unwrap_or_else(|| Fault::sentence("failing")),
        )),
    }
}

/// One usage window of one account: its label, length in minutes (0 when
/// unstated), the share used (0 to 1) and when it resets.
struct Window {
    label: String,
    minutes: i64,
    used: f64,
    resets_at: Option<Timestamp>,
}

/// An account's usage windows, in every shape `ck quota` reads: the three
/// slots and the `extraRateWindows` list, whose numbers sit under `window`
/// and whose label is its `title`.
fn windows_of(entry: &Value) -> Vec<Window> {
    quota::quota_window_rows_for_entry(entry)
        .into_iter()
        .filter_map(|(label, w)| {
            let used = quota::quota_window_used_percent(&w)? / 100.0;
            Some(Window {
                label,
                minutes: w.get("windowMinutes").and_then(Value::as_i64).unwrap_or(0),
                used,
                resets_at: w
                    .get("resetsAt")
                    .and_then(Value::as_str)
                    .and_then(parse_rfc3339),
            })
        })
        .collect()
}

/// A provider's credit pools added up: the remaining and total amounts of
/// pools that share one unit (with the count when there is more than one),
/// and the soonest expiry. Pools in different units are only counted.
pub fn pools_summary(pools: &[quota::SpendPool]) -> Option<(String, Option<Timestamp>)> {
    if pools.is_empty() {
        return None;
    }
    let unit_of = |p: &quota::SpendPool| {
        p.remaining
            .as_ref()
            .or(p.total.as_ref())
            .map(|a| a.2.clone())
            .unwrap_or_default()
    };
    let unit = unit_of(&pools[0]);
    let expires = pools
        .iter()
        .filter_map(|p| p.expires_at.as_deref().and_then(parse_rfc3339))
        .min();
    let n = pools.len();
    if pools.iter().any(|p| unit_of(p) != unit) {
        return Some((format!("{n} credit pools"), expires));
    }
    // Add in minor units at the largest exponent any pool uses.
    let exp = pools
        .iter()
        .flat_map(|p| [p.remaining.as_ref(), p.total.as_ref()])
        .flatten()
        .map(|a| a.1)
        .max()
        .unwrap_or(0)
        .clamp(0, 18);
    let scaled = |a: &(i64, i64, String)| {
        let up = 10_i64
            .checked_pow((exp - a.1.clamp(0, 18)) as u32)
            .unwrap_or(1);
        a.0.saturating_mul(up)
    };
    let sum = |pick: fn(&quota::SpendPool) -> Option<&(i64, i64, String)>| -> Option<i64> {
        pools
            .iter()
            .map(|p| pick(p).map(scaled))
            .sum::<Option<i64>>()
    };
    let left = sum(|p| p.remaining.as_ref());
    let cap = sum(|p| p.total.as_ref());
    let amount = |minor: i64| {
        money(
            &separated(&crate::metrics::format_minor_amount(minor, exp)),
            &unit,
        )
    };
    let mut text = match (left, cap) {
        (Some(l), Some(c)) => format!("{} of {} left", amount(l), amount(c)),
        (Some(l), None) => format!("{} left", amount(l)),
        (None, Some(c)) => format!("limit {}", amount(c)),
        (None, None) => format!("{n} credit pools"),
    };
    if n > 1 && !text.ends_with("credit pools") {
        text += &format!(" · {n} pools");
    }
    Some((text, expires))
}

/// Thousands separators in a decimal amount's whole part: "1000.00" is
/// "1,000.00".
fn separated(amount: &str) -> String {
    let (sign, rest) = match amount.strip_prefix('-') {
        Some(r) => ("-", r),
        None => ("", amount),
    };
    let (whole, frac) = rest
        .split_once('.')
        .map_or((rest, None), |(w, f)| (w, Some(f)));
    let mut grouped = String::new();
    for (i, ch) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    match frac {
        Some(f) => format!("{sign}{grouped}.{f}"),
        None => format!("{sign}{grouped}"),
    }
}

fn money(amount: &str, unit: &str) -> String {
    match unit {
        "USD" => format!("${amount}"),
        "EUR" => format!("€{amount}"),
        "GBP" => format!("£{amount}"),
        "" => amount.to_string(),
        other => format!("{amount} {other}"),
    }
}

/// The usage panel's providers from insula's `usage.get` reply, grouped by
/// provider in reply order and read with the same row parser `ck quota`
/// uses. A provider's bar is the window closest to its limit: among the
/// windows its readable accounts report, the one with the highest average
/// share used across the accounts reporting it. A failing account is listed
/// as failing and left out of the sum; a provider with no readable account
/// carries an error instead of a share; one whose accounts report only
/// credit shows what is left of it.
pub fn usage(reply: &Value, now: Timestamp) -> Result<Vec<Provider>, Fault> {
    if let Some(err) = reply_error(reply) {
        return Err(err);
    }
    let rows = unwrap_result(reply)
        .as_array()
        .ok_or_else(|| Fault::sentence("unexpected usage.get reply"))?;
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<&Value>> = HashMap::new();
    for row in rows {
        let id = quota::provider_id(row);
        if !groups.contains_key(&id) {
            order.push(id.clone());
        }
        groups.entry(id).or_default().push(row);
    }
    let mut providers = Vec::new();
    for id in order {
        let mut accounts = Vec::new();
        let mut all_windows: Vec<Window> = Vec::new();
        let mut pools: Vec<quota::SpendPool> = Vec::new();
        let mut first_failure: Option<Fault> = None;
        for entry in &groups[&id] {
            let failure = match account_failure(entry) {
                Err(()) => continue,
                Ok(f) => f,
            };
            let windows = if failure.is_none() {
                windows_of(entry)
            } else {
                Vec::new()
            };
            let own_pools = if failure.is_none() {
                quota::spend_pools(entry)
            } else {
                Vec::new()
            };
            if first_failure.is_none() {
                first_failure = failure.clone();
            }
            let stale = entry.get("stale").is_some_and(|s| !s.is_null());
            let note = if stale {
                "last good reading".to_string()
            } else if windows.is_empty() {
                pools_summary(&own_pools).map(|p| p.0).unwrap_or_default()
            } else {
                String::new()
            };
            accounts.push(Account {
                who: account_who(entry),
                windows: windows
                    .iter()
                    .map(|w| UsageWindow {
                        name: w.label.clone(),
                        used: w.used,
                        resets_at: w.resets_at,
                    })
                    .collect(),
                note,
                error: failure.map(|f| f.text),
            });
            if windows.is_empty() {
                pools.extend(own_pools);
            }
            all_windows.extend(windows);
        }
        if accounts.is_empty() {
            continue;
        }
        // Pick the one usage window to show for this provider: the label
        // whose windows are, on average across the provider's accounts, the
        // most used (closest to 100% of their quota). A tie goes to the
        // longer window.
        let mut labels: Vec<(&str, i64)> = Vec::new();
        for w in &all_windows {
            if !labels.iter().any(|(l, _)| *l == w.label) {
                labels.push((&w.label, w.minutes));
            }
        }
        let share = |label: &str| {
            let of: Vec<&Window> = all_windows.iter().filter(|w| w.label == label).collect();
            of.iter().map(|w| w.used).sum::<f64>() / of.len().max(1) as f64
        };
        let chosen = labels
            .iter()
            .copied()
            .reduce(|best, cand| {
                let (b, c) = (share(best.0), share(cand.0));
                if c > b || (c == b && cand.1 > best.1) {
                    cand
                } else {
                    best
                }
            })
            .map(|(l, _)| l.to_string());
        let summary = pools_summary(&pools);
        let (window, used, of, resets_at, error) = match &chosen {
            Some(label) => {
                let ws: Vec<&Window> = all_windows.iter().filter(|w| &w.label == label).collect();
                let resets = ws
                    .iter()
                    .filter_map(|w| w.resets_at)
                    .filter(|t| *t > now)
                    .min();
                (
                    label.clone(),
                    Some(ws.iter().map(|w| w.used).sum::<f64>()),
                    ws.len() as u32,
                    resets,
                    None,
                )
            }
            None if summary.is_some() => (String::new(), None, 0, None, None),
            None => {
                let err = first_failure
                    .clone()
                    .unwrap_or_else(|| Fault::sentence("no usage reported"));
                (String::new(), None, 0, None, Some(err))
            }
        };
        let (balance, balance_expires) = match summary {
            Some((text, expires)) if chosen.is_none() => (Some(text), expires),
            _ => (None, None),
        };
        providers.push(Provider {
            name: quota::format_provider_display_name(&id),
            window,
            used,
            of,
            unit: if of == 1 { "account" } else { "accounts" }.into(),
            resets_at,
            accounts,
            error_kind: error.as_ref().map(|f| f.kind).unwrap_or_default(),
            error: error.map(|f| f.text),
            balance,
            balance_expires,
        });
    }
    Ok(providers)
}

// ---- devices -------------------------------------------------------------------

/// Paired devices from callosum's `callosum.devices_read` reply. Online
/// means an established tunnel.
pub fn devices(reply: &Value, now: Timestamp) -> Result<Devices, Fault> {
    if let Some(err) = reply_error(reply) {
        return Err(err);
    }
    let body = unwrap_result(reply);
    let list = body
        .get("devices")
        .and_then(Value::as_array)
        .ok_or_else(|| Fault::sentence("unexpected devices_read reply"))?;
    let mut out = Vec::new();
    for d in list {
        if d.get("trust").and_then(Value::as_str) == Some("tombstoned") {
            continue;
        }
        let name = d
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unnamed")
            .to_string();
        let tunnel = d.get("tunnel").and_then(Value::as_str).unwrap_or("");
        let online = tunnel == "established";
        let seen = d
            .get("last_seen_unix_ms")
            .and_then(Value::as_i64)
            .map(|ms| {
                let age = now.secs_since(Timestamp::from_unix_millis(ms));
                if age < 60.0 {
                    "now".to_string()
                } else {
                    format!("{} ago", ago(age))
                }
            })
            .unwrap_or_else(|| "unknown".into());
        let feeds = match d.get("subscriptions_live").and_then(Value::as_u64) {
            Some(0) => "—".to_string(),
            Some(1) => "1 live feed".to_string(),
            Some(n) => format!("{n} live feeds"),
            None => "unknown".to_string(),
        };
        out.push(Device {
            short_name: name.strip_prefix("CortexKit ").unwrap_or(&name).to_string(),
            kind: d
                .get("device_class")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            status: if online { Status::Ok } else { Status::Stopped },
            path: d
                .get("carrier")
                .and_then(Value::as_str)
                .unwrap_or(if online { "unknown" } else { "offline" })
                .to_string(),
            seen,
            feeds,
            name,
        });
    }
    // Online devices first, so a row with room for only some devices shows
    // the online ones.
    out.sort_by_key(|d| d.status != Status::Ok);
    let online = out.iter().filter(|d| d.status == Status::Ok).count() as u32;
    Ok(Devices { list: out, online })
}

// ---- agents --------------------------------------------------------------------

/// One live agent's identity from entorhinal's `agent.fleet_identity`.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentIdentity {
    pub id: String,
    pub name: String,
    pub project: String,
}

/// The identities in an `agent.fleet_identity` reply.
pub fn agent_identities(reply: &Value) -> Result<Vec<AgentIdentity>, Fault> {
    if let Some(err) = reply_error(reply) {
        return Err(err);
    }
    let body = unwrap_result(reply);
    let list = body
        .get("agents")
        .and_then(Value::as_array)
        .ok_or_else(|| Fault::sentence("unexpected fleet_identity reply"))?;
    Ok(list
        .iter()
        .filter_map(|a| {
            let id = a.get("agentId").and_then(Value::as_str)?.to_string();
            let name = a
                .get("displayName")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(&id)
                .to_string();
            let project = a
                .get("project")
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some(AgentIdentity { id, name, project })
        })
        .collect())
}

/// An agent's state from prefrontal-core's `agent.fleet_residence`.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentResidence {
    /// "busy", "idle" or "unknown".
    pub activity: String,
    pub activity_at: Option<Timestamp>,
    pub doing: Option<String>,
}

pub fn agent_residences(reply: &Value) -> Result<HashMap<String, AgentResidence>, Fault> {
    if let Some(err) = reply_error(reply) {
        return Err(err);
    }
    let body = unwrap_result(reply);
    let list = body
        .get("agents")
        .and_then(Value::as_array)
        .ok_or_else(|| Fault::sentence("unexpected fleet_residence reply"))?;
    Ok(list
        .iter()
        .filter_map(|a| {
            let id = a.get("agentId").and_then(Value::as_str)?.to_string();
            Some((
                id,
                AgentResidence {
                    activity: a
                        .get("activity")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                    activity_at: a
                        .get("activityAtMs")
                        .and_then(Value::as_i64)
                        .map(Timestamp::from_unix_millis),
                    doing: a
                        .get("board")
                        .and_then(|b| b.get("statusText"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                },
            ))
        })
        .collect())
}

/// Where each agent lives, from entorhinal's `agent.where_many`: the holding
/// machine's name, or `here` (this machine's label) when it is held here.
pub fn agent_places(reply: &Value, here: &str) -> Result<HashMap<String, String>, Fault> {
    if let Some(err) = reply_error(reply) {
        return Err(err);
    }
    let body = unwrap_result(reply);
    let place = |id: &str, row: &Value| -> Option<(String, String)> {
        let status = row.get("status").and_then(Value::as_str).unwrap_or("");
        let holder = row.get("holder_name").and_then(Value::as_str);
        let name = match (status, holder) {
            ("held_here", _) => here.to_string(),
            (_, Some(h)) if !h.is_empty() => h.to_string(),
            ("unclaimed", _) => "unclaimed".to_string(),
            _ => return None,
        };
        Some((id.to_string(), name))
    };
    let rows = body
        .get("agents")
        .or_else(|| body.get("results"))
        .unwrap_or(body);
    let mut out = HashMap::new();
    if let Some(list) = rows.as_array() {
        for row in list {
            let id = row
                .get("agent_id")
                .or_else(|| row.get("agentId"))
                .and_then(Value::as_str);
            if let Some(id) = id {
                out.extend(place(id, row));
            }
        }
    } else if let Some(map) = rows.as_object() {
        for (id, row) in map {
            out.extend(place(id, row));
        }
    }
    Ok(out)
}

/// Workspace and project counts from entorhinal's `enumerate`: each is a
/// list (counted) or a number.
pub fn fleet_counts(reply: &Value) -> Result<(Option<u32>, Option<u32>), Fault> {
    if let Some(err) = reply_error(reply) {
        return Err(err);
    }
    let body = unwrap_result(reply);
    let count = |k: &str| -> Option<u32> {
        let v = body.get(k)?;
        v.as_array()
            .map(|a| a.len() as u32)
            .or_else(|| v.as_u64().map(|n| n as u32))
    };
    Ok((count("workspaces"), count("projects")))
}

/// The agents panel: identity on the left, joined on exact agent id with
/// state and place. With no state for an agent (none read yet, a refused
/// source, or an agent the reply leaves out), its state is "unknown",
/// never idle.
pub fn agents(
    identities: &[AgentIdentity],
    residences: Option<&HashMap<String, AgentResidence>>,
    places: Option<&HashMap<String, String>>,
    counts: (Option<u32>, Option<u32>),
    now: Timestamp,
) -> Agents {
    let mut list: Vec<Agent> = identities
        .iter()
        .map(|i| {
            let r = residences.and_then(|r| r.get(&i.id));
            let state = match r.map(|r| r.activity.as_str()) {
                Some("busy") => "working".to_string(),
                Some("idle") => match r.and_then(|r| r.activity_at) {
                    Some(t) => format!("idle {}", ago(now.secs_since(t).max(0.0))),
                    None => "idle".to_string(),
                },
                _ => "unknown".to_string(),
            };
            Agent {
                name: i.name.clone(),
                project: i.project.clone(),
                state,
                machine: places
                    .and_then(|p| p.get(&i.id))
                    .cloned()
                    .unwrap_or_else(|| "unknown".into()),
                doing: r.and_then(|r| r.doing.clone()),
            }
        })
        .collect();
    // Working agents first, then idle, then unknown; by name within each.
    list.sort_by_key(|a| (!a.working(), !a.state_known(), a.name.clone()));
    let working = list.iter().filter(|a| a.working()).count() as u32;
    let machines = places.map(|p| {
        let mut held: Vec<&String> = identities
            .iter()
            .filter_map(|i| p.get(&i.id))
            .filter(|m| m.as_str() != "unclaimed")
            .collect();
        held.sort();
        held.dedup();
        held.len() as u32
    });
    Agents {
        list,
        working,
        machines,
        workspaces: counts.0,
        projects: counts.1,
    }
}

// ---- activity ------------------------------------------------------------------

/// Who asked for an entry: the agent's display name when the actor carries
/// an agent id, or the id itself while identity has no name for it; the
/// module that acted; "direct" for a direct connection; "unverified" for an
/// unverified one. Never "you": the daemon can't tell a person from a
/// script.
pub fn actor(e: &WireActivity, names: &HashMap<String, String>) -> String {
    match (&e.actor.agent_id, &e.actor.principal) {
        (Some(id), _) => names.get(id).cloned().unwrap_or_else(|| id.clone()),
        (None, Principal::Reserved { module_id }) => module_id.clone(),
        (None, Principal::Direct) => "direct".into(),
        (None, Principal::Unverified) => "unverified".into(),
    }
}

/// An activity entry as Home lists it: the subject it names (the module a
/// lifecycle entry is about, else the module that recorded it), what
/// happened, and who asked for it ([`actor`]).
pub fn activity_entry(e: &WireActivity, names: &HashMap<String, String>) -> ActivityEntry {
    let subject = match &e.subject {
        Some(s) if s.subject_type == "module" => s.id.clone(),
        _ => e.module.clone(),
    };
    let module_subject = e
        .subject
        .as_ref()
        .is_some_and(|s| s.subject_type == "module");
    let what = match e.summary.strip_prefix("module ") {
        Some(rest) if module_subject => rest.to_string(),
        _ => e.summary.clone(),
    };
    ActivityEntry {
        at: Timestamp::from_unix_millis(e.at_ms as i64),
        who: actor(e, names),
        subject,
        what,
        kind: kind_label(&e.kind),
    }
}

/// A short label for an activity kind: every `subc.module_*` kind is
/// "module"; any other is its text before the first `.` or `_`.
pub fn kind_label(kind: &str) -> String {
    if kind.starts_with("subc.module_") {
        return "module".into();
    }
    kind.split(['.', '_']).next().unwrap_or(kind).to_string()
}

const RESTARTED: &str = "subc.module_restarted";
const STARTED: &str = "subc.module_started";

/// The longest gap between a restart entry and the start entry it is paired
/// with.
const PAIR_WINDOW_MS: u64 = 120_000;

fn module_id_of(e: &WireActivity) -> Option<&str> {
    e.subject
        .as_ref()
        .filter(|s| s.subject_type == "module")
        .map(|s| s.id.as_str())
}

/// A start entry's spawn evidence: the executable it runs now and the one
/// before, each `None` when absent or `null`.
fn evidence(started: &WireActivity) -> (Option<&Value>, Option<&Value>) {
    let get = |k: &str| started.details.get(k).filter(|v| !v.is_null());
    (get("new_evidence"), get("old_evidence"))
}

/// The first 8 characters of an evidence's `digest`, when it has a string
/// one. Only Linux evidence (`linux_proc_sha256`) carries a digest.
fn digest8(evidence: &Value) -> Option<String> {
    evidence
        .get("digest")
        .and_then(Value::as_str)
        .map(|d| d.chars().take(8).collect())
}

/// Whether a start entry's details say the process runs a different
/// executable than before: both evidences are present and differ, whatever
/// their method (a digest, a device and inode, a volume and file id, or a
/// method this build doesn't know).
pub fn new_binary(started: &WireActivity) -> bool {
    matches!(evidence(started), (Some(new), Some(old)) if new != old)
}

/// What a restart did, as the Activity list says it after the subject:
/// "restarted on build <first 8 of the new digest>" when the start's new
/// evidence has a digest, else "restarted · new binary" when the executable
/// changed ([`new_binary`]), else "restarted".
pub fn restart_what(started: &WireActivity) -> String {
    let (new, _) = evidence(started);
    match new.and_then(digest8) {
        Some(d) => format!("restarted on build {d}"),
        None if new_binary(started) => "restarted · new binary".into(),
        None => "restarted".into(),
    }
}

/// The Build row for a start entry: "<old 8> → <new 8>" when both evidences
/// have a digest, the new digest's 8 alone when only it does, and nothing
/// when the new evidence has none.
pub fn restart_build(started: &WireActivity) -> Option<String> {
    let (new, old) = evidence(started);
    let new = new.and_then(digest8)?;
    Some(match old.and_then(digest8) {
        Some(old) => format!("{old} → {new}"),
        None => new,
    })
}

/// Pairs each restart entry with the start entry the daemon recorded for the
/// same module up to two minutes later. `entries` run newest first, so a
/// restart's start sits before it. Returns the start paired with each
/// restart, by index, and which entries were taken as a start.
fn restart_pairs(entries: &[WireActivity]) -> (HashMap<usize, usize>, Vec<bool>) {
    let mut partner: HashMap<usize, usize> = HashMap::new();
    let mut taken = vec![false; entries.len()];
    for (i, e) in entries.iter().enumerate() {
        if e.kind != RESTARTED {
            continue;
        }
        let Some(id) = module_id_of(e) else { continue };
        let found = (0..i).rev().find(|&j| {
            let s = &entries[j];
            !taken[j]
                && s.kind == STARTED
                && module_id_of(s) == Some(id)
                && s.at_ms >= e.at_ms
                && s.at_ms - e.at_ms <= PAIR_WINDOW_MS
        });
        if let Some(j) = found {
            taken[j] = true;
            partner.insert(i, j);
        }
    }
    (partner, taken)
}

/// Home's activity rows from ledger entries, newest first. The daemon
/// records a restart as `subc.module_restarted` and then, seconds later,
/// `subc.module_started` for the same module; the pair reads as one
/// "restarted" row, with " · new binary" when the start ran a different
/// executable.
pub fn activity_rows(
    entries: &[WireActivity],
    names: &HashMap<String, String>,
) -> Vec<ActivityEntry> {
    let (partner, taken) = restart_pairs(entries);
    entries
        .iter()
        .enumerate()
        .filter(|(i, _)| !taken[*i])
        .map(|(i, e)| {
            let mut row = activity_entry(e, names);
            if let Some(&j) = partner.get(&i) {
                row.what = if new_binary(&entries[j]) {
                    "restarted · new binary".into()
                } else {
                    "restarted".into()
                };
            }
            row
        })
        .collect()
}

/// The Activity list's rows for one contiguous run of ledger entries, newest
/// first, with each restart and its start read as one row. A merged row is
/// the restart's (its seq, kind, time and details) and takes its summary and
/// Build row from the start's evidence. A start whose restart isn't loaded
/// reads alone, with the Build row its own evidence gives.
pub fn ledger_rows(entries: &[WireActivity], names: &HashMap<String, String>) -> Vec<ActivityRow> {
    let (partner, taken) = restart_pairs(entries);
    entries
        .iter()
        .enumerate()
        .filter(|(i, _)| !taken[*i])
        .map(|(i, e)| {
            let base = activity_entry(e, names);
            let started = partner.get(&i).map(|&j| &entries[j]);
            let (what, build) = match started {
                Some(s) => (restart_what(s), restart_build(s)),
                None if e.kind == STARTED => (base.what, restart_build(e)),
                None => (base.what, None),
            };
            ActivityRow {
                key: entry_key(e),
                partner: started.map(entry_key),
                row: ActivityRowKind::Entry,
                at: base.at,
                subject: base.subject,
                what,
                who: Some(base.who),
                by: None,
                by_agent: e.actor.agent_id.is_some(),
                kind: Some(e.kind.clone()),
                seq: Some(e.seq),
                build,
                details: Some(e.details.clone()),
            }
        })
        .collect()
}

/// A ledger entry's identity: its daemon incarnation and seq.
pub fn entry_key(e: &WireActivity) -> ActivityKey {
    ActivityKey::Entry {
        incarnation: e.daemon_incarnation.clone(),
        seq: e.seq,
    }
}

/// A process exit as an Activity row: no seq, actor or details.
pub fn exit_row(e: &ProcessEvent) -> ActivityRow {
    ActivityRow {
        key: ActivityKey::Exit {
            module: e.module.clone(),
            at: e.at,
        },
        partner: None,
        row: ActivityRowKind::Exit,
        at: e.at,
        subject: e.module.clone(),
        what: e.what.clone(),
        who: None,
        by: None,
        by_agent: false,
        kind: None,
        seq: None,
        build: None,
        details: None,
    }
}

/// The label row between two loaded ranges whose entries in between are no
/// longer kept, below the entry with `below_seq`.
pub fn gap_row(below_seq: u64, at: Timestamp) -> ActivityRow {
    ActivityRow {
        key: ActivityKey::Gap { below_seq },
        partner: None,
        row: ActivityRowKind::Gap,
        at,
        subject: String::new(),
        what: "Entries here are no longer kept".into(),
        who: None,
        by: None,
        by_agent: false,
        kind: None,
        seq: None,
        build: None,
        details: None,
    }
}

/// Process exits, newest first: the spawn feed's exits and the ones the
/// daemon retained (`supervisor.terminals`). The same exit in both (same
/// module, within 10 s) is listed once, as the retained one, which says
/// what the supervisor did next.
pub fn process_exits(spawn: &[ProcessEvent], retained: &[ProcessEvent]) -> Vec<ProcessEvent> {
    let exited = |e: &&ProcessEvent| e.kind == ProcessEventKind::Exited;
    let mut out: Vec<ProcessEvent> = retained.iter().filter(exited).cloned().collect();
    for e in spawn.iter().filter(exited) {
        if !out.iter().any(|x| same_event(x, e)) {
            out.push(e.clone());
        }
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.at));
    out
}

/// Whether two process events are one event seen by two sources: the same
/// module and kind, within 10 s of each other. Spawn events carry a pid and
/// retained exits don't, so the time is what ties them.
pub fn same_event(a: &ProcessEvent, b: &ProcessEvent) -> bool {
    const SAME_MS: i64 = 10_000;
    a.module == b.module
        && a.kind == b.kind
        && (a.at.unix_millis() - b.at.unix_millis()).abs() <= SAME_MS
}

// ---- logs ----------------------------------------------------------------------

/// A level word at the start of a log line ("INFO", "WARN", "error",
/// "level=warn"), after any leading timestamp, and the rest of the line.
pub fn parse_log_text(text: &str) -> (LogLevel, String) {
    let (_, level, message) = parse_log_line(text);
    (level, message)
}

/// A log line's leading RFC 3339 timestamp (as tracing writes it), its
/// level word, and the rest. The `[module]` prefix the daemon's stderr
/// capture adds is dropped. A line with no level word is info and keeps all
/// its remaining text.
pub fn parse_log_line(text: &str) -> (Option<Timestamp>, LogLevel, String) {
    let mut rest = text.trim_end();
    let mut at = None;
    // Up to two leading tokens: a timestamp, and the `[module]` prefix, in
    // either order.
    for _ in 0..2 {
        let Some((first, after)) = rest.split_once(char::is_whitespace) else {
            break;
        };
        let inner = first.trim_start_matches('[').trim_end_matches(']');
        if at.is_none() {
            if let Some(t) = parse_rfc3339(inner) {
                at = Some(t);
                rest = after.trim_start();
                continue;
            }
        }
        if first.len() > 2
            && first.starts_with('[')
            && first.ends_with(']')
            && level_word(inner).is_none()
        {
            rest = after.trim_start();
            continue;
        }
        break;
    }
    let (first, after) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let word = first
        .trim_start_matches("level=")
        .trim_matches(|c| c == '[' || c == ']' || c == ':');
    match level_word(word) {
        Some(level) => (at, level, after.trim_start().to_string()),
        None => (at, LogLevel::Info, rest.to_string()),
    }
}

fn level_word(word: &str) -> Option<LogLevel> {
    match word.to_ascii_lowercase().as_str() {
        "warn" | "warning" => Some(LogLevel::Warn),
        "error" | "err" | "fatal" => Some(LogLevel::Error),
        "info" | "debug" | "trace" => Some(LogLevel::Info),
        _ => None,
    }
}

/// A module's retained exits from `supervisor.terminals` as start-and-exit
/// rows: how it exited, and what the supervisor did next.
pub fn terminal_events(
    module: &str,
    history: &subc_control::TerminalHistory,
) -> Vec<crate::model::ProcessEvent> {
    history
        .entries
        .iter()
        .map(|t| {
            let how = match (t.exit_code, t.exit_signal) {
                (Some(0), _) => "exited cleanly".to_string(),
                (Some(code), _) => format!("exited with code {code}"),
                (None, Some(sig)) => format!("exited on signal {sig}"),
                (None, None) => "exited".to_string(),
            };
            let then = serde_json::to_value(&t.disposition)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            crate::model::ProcessEvent {
                at: Timestamp::from_unix_millis(t.at_ms as i64),
                module: module.to_string(),
                what: if then.is_empty() {
                    how
                } else {
                    format!("{how} · {then}")
                },
                kind: crate::model::ProcessEventKind::Exited,
            }
        })
        .collect()
}

/// A module's merged log lines (see `DaemonService::module_log`) as the log
/// view shows them: the line's own time and level when it states them, the
/// text without its leading timestamp, `[module]` prefix and level word.
pub fn merged_log_lines(lines: &[super::ModuleLogLine]) -> Vec<LogLine> {
    lines
        .iter()
        .map(|l| {
            let (stamped, parsed_level, message) = parse_log_line(&l.text);
            LogLine {
                at: l.at_ms.map(Timestamp::from_unix_millis).or(stamped),
                level: l.level.unwrap_or(parsed_level),
                message,
            }
        })
        .collect()
}

/// A module's retained stderr as log lines, oldest first. A process start
/// marks where the new process's lines begin.
pub fn log_lines(entries: &[StderrTailEntry]) -> Vec<LogLine> {
    let mut out = Vec::new();
    for e in entries {
        match e {
            StderrTailEntry::Line { text, at_ms, .. } => {
                let (stamped, level, message) = parse_log_line(text);
                let at = at_ms
                    .map(|ms| Timestamp::from_unix_millis(ms as i64))
                    .or(stamped);
                out.push(LogLine { at, level, message });
            }
            StderrTailEntry::ProcessStart => out.push(LogLine {
                at: None,
                level: LogLevel::Info,
                message: "── a new process started ──".into(),
            }),
            StderrTailEntry::Unknown { .. } => {}
        }
    }
    out
}

// ---- time ----------------------------------------------------------------------

/// Parses RFC 3339 times: `2026-10-20T18:43:02Z`, with any fraction of a
/// second and a `Z` or `±HH:MM` offset.
pub fn parse_rfc3339(s: &str) -> Option<Timestamp> {
    let s = s.trim();
    if s.len() < 20 {
        return None;
    }
    let (date, rest) = s.split_at(10);
    let rest = rest.strip_prefix(['T', 't', ' '])?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    let (time, offset_secs) = if let Some(t) = rest.strip_suffix(['Z', 'z']) {
        (t, 0i64)
    } else {
        let pos = rest.rfind(['+', '-'])?;
        let (t, off) = rest.split_at(pos);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let (h, m) = off[1..].split_once(':')?;
        let secs = h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().ok()? * 60;
        (t, sign * secs)
    };
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next()?.parse().ok()?;
    let second: i64 = t.next()?.parse().ok()?;
    if month == 0 || month > 12 || day == 0 || day > 31 || hour > 23 || minute > 59 {
        return None;
    }
    let millis: i64 = if frac.is_empty() {
        0
    } else {
        let digits: String = frac.chars().take(3).collect();
        if !frac.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        format!("{digits:0<3}").parse().ok()?
    };
    let base = Timestamp::parse_utc(&format!("{year:04}-{month:02}-{day:02}T00:00:00Z"))?;
    Some(base.add_millis((hour * 3600 + minute * 60 + second - offset_secs) * 1000 + millis))
}

/// Names for agent ids, for the activity list's "who".
pub fn agent_names(identities: &[AgentIdentity]) -> HashMap<String, String> {
    identities
        .iter()
        .map(|i| (i.id.clone(), i.name.clone()))
        .collect()
}

/// Consumers of a module's open routes, by name, from `supervisor.routes`.
pub fn route_consumers(routes: &subc_control::SupervisorRouteModule) -> Vec<String> {
    let mut out: Vec<String> = routes
        .routes
        .iter()
        .filter(|r| !r.draining)
        .map(|r| consumer_name(&r.consumer))
        .collect();
    out.sort();
    out
}

fn consumer_name(c: &subc_control::SupervisorRouteConsumer) -> String {
    use subc_control::SupervisorRouteConsumer as C;
    match c {
        C::Reserved { module_id } => module_id.clone(),
        C::Direct { .. } => "direct".into(),
        C::Unknown { tag, .. } => tag.clone(),
    }
}
