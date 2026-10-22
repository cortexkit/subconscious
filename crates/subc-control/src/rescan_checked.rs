//! Wire shapes for `supervisor.rescan_checked`: a rescan preview computed on
//! configuration text the caller supplies, which nothing is written from, and
//! an apply that runs only while the daemon still matches that preview.

use serde::{Deserialize, Serialize};

/// Advertised in `server.describe` by a daemon that serves
/// `supervisor.rescan_checked`. A client sends that op only when this is
/// listed: an older daemon refuses the unknown op, so a client that skipped
/// the check would fail closed rather than preview the file on disk.
pub const RESCAN_CANDIDATE_PREVIEW_CAPABILITY: &str = "rescan-candidate-preview/v1";

/// The largest candidate text, in bytes, a preview accepts. A daemon config
/// file is a few kilobytes; the cap bounds what one request can make the
/// daemon parse.
pub const RESCAN_CANDIDATE_MAX_BYTES: usize = 1024 * 1024;

/// Refusal code for an apply whose `expect` no longer matches the daemon: the
/// file it reads hashes differently, or the staged generation has moved since
/// the preview (another rescan with module changes ran, even one that failed
/// partway, or the daemon restarted). Nothing was applied.
pub const CONFIG_CHANGED_SINCE_PREVIEW: &str = "config_changed_since_preview";

/// Refusal code for a preview whose candidate is longer than
/// [`RESCAN_CANDIDATE_MAX_BYTES`].
pub const RESCAN_CANDIDATE_TOO_LARGE: &str = "candidate_too_large";

/// What must still match for the daemon to accept an apply of this preview,
/// so the apply does what the preview described.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RescanExpect {
    /// Lowercase hex SHA-256 of the candidate text the preview parsed. The
    /// apply hashes the file the daemon reads and refuses on any difference,
    /// so the bytes applied are exactly the bytes previewed.
    pub config_sha256: String,
    /// The daemon's staged configuration at preview time.
    pub staged_generation: StagedGeneration,
}

/// Names one state of the daemon's staged configuration. The counter alone is
/// not enough: it starts again at zero when the daemon restarts, and a
/// restarted daemon may hold different staged specs under the same number.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StagedGeneration {
    /// The `daemon_incarnation` the daemon stamps on its activity ledger and
    /// spawn events: one value per daemon process lifetime.
    pub incarnation: String,
    /// Moves each time an executed rescan sets out to change the supervised
    /// modules' staged or applied configuration within that incarnation. It
    /// moves before the first change, so it also moves when that rescan fails
    /// partway.
    pub counter: u64,
}

open_string_enum! {
    /// What applying a candidate would do to one module.
    RescanModuleActionKind {
        /// The launch inputs change and the module is running or due to run,
        /// so it needs a restart to use them. A rescan only stages the new
        /// inputs; it does not restart the module itself.
        Restart => "restart",
        /// No restart is needed: a health policy edit takes effect at once,
        /// and a module that stays disabled uses its changed launch inputs
        /// when it next starts.
        Reload => "reload",
        /// The module is enabled or added enabled, so the rescan starts it.
        Start => "start",
        /// The module is disabled or removed while enabled, so the rescan
        /// stops it.
        Stop => "stop",
    }
}

/// One module's entry in a candidate preview.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RescanModuleAction {
    pub module_id: String,
    pub action: RescanModuleActionKind,
    /// For `restart` and `reload`: which parts of the module's entry changed,
    /// from `program`, `arguments`, `environment`, `protocol`, `reserved`,
    /// `reserved_prefixes`, `overlap` and `health`. A newer daemon may send
    /// other names, so readers must not treat this list as complete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<String>,
    /// For `restart` and `stop`: how long, in ms, the module's drain will wait
    /// for in-flight work to finish before it is torn down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain_timeout_ms: Option<u64>,
    /// For `restart` and `stop`: each busy gauge the module's registration
    /// declares, in declaration order, with its value from the daemon's
    /// cached health metrics. The drain waits for these gauges to read zero.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub busy_gauges: Vec<BusyGaugeReading>,
}

/// A declared busy gauge's last reading from the daemon's health cache.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BusyGaugeReading {
    pub gauge: String,
    /// The value the cached health metrics hold for the gauge, as the module
    /// sent it. Absent when they hold none, which includes a module that never
    /// reported the gauge and a metrics object the cache replaced because it
    /// was too large.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientControlRequest, SupervisorRescanResult};

    #[test]
    fn an_action_a_newer_daemon_sends_round_trips_as_unknown() {
        let wire = r#"{"module_id":"m","action":"swap"}"#;
        let action: RescanModuleAction = serde_json::from_str(wire).unwrap();
        assert_eq!(
            action.action,
            RescanModuleActionKind::Unknown("swap".to_string())
        );
        assert_eq!(serde_json::to_string(&action).unwrap(), wire);
    }

    #[test]
    fn a_plain_rescan_reply_decodes_without_the_preview_fields() {
        let wire = r#"{"added":[],"removed":[],"changed_pending_reload":[],"unchanged":1}"#;
        let result: SupervisorRescanResult = serde_json::from_str(wire).unwrap();
        assert!(result.module_actions.is_empty());
        assert!(result.earlier_pending.is_empty());
        assert_eq!(result.expect, None);
        assert_eq!(serde_json::to_string(&result).unwrap(), wire);
    }

    /// The op has no default form: a body that leaves `preview` out says
    /// neither which of the two forms it is, so it must not decode as one.
    #[test]
    fn a_checked_rescan_without_preview_does_not_decode() {
        let wire = r#"{"op":"supervisor.rescan_checked","candidate":"{}"}"#;
        assert!(serde_json::from_str::<ClientControlRequest>(wire).is_err());
    }
}
