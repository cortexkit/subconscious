//! Wire shapes for `app.open`, which asks the daemon to start a known operator
//! app from its installed location. They are standalone bodies, kept out of
//! the exhaustive control enums so adding them changes no existing match.
//!
//! The daemon answers with an [`AppOpenReply`] or an error frame carrying one
//! of the codes below. It checks, in order: whether the platform uses
//! `app.open` at all ([`APP_OPEN_NOT_USED`]), whether it is switched on
//! ([`APP_OPEN_DISABLED`]), whether the app is known ([`APP_UNKNOWN`]),
//! whether the app is already running (an `already_running` reply), whether it
//! is installed ([`APP_NOT_INSTALLED`]), and whether the installed binary
//! passes verification ([`APP_VERIFICATION_FAILED`]).

use serde::{Deserialize, Serialize};

/// Refusal code on a platform that never launches apps through the daemon.
pub const APP_OPEN_NOT_USED: &str = "app_open_not_used";
/// Refusal code when the platform supports `app.open` but this daemon build
/// has it switched off.
pub const APP_OPEN_DISABLED: &str = "app_open_disabled";
/// Refusal code for an app id the daemon does not know.
pub const APP_UNKNOWN: &str = "app_unknown";
/// Refusal code for a known app that is not installed where the daemon looks.
pub const APP_NOT_INSTALLED: &str = "app_not_installed";
/// Refusal code for an installed binary that failed the daemon's checks. No
/// process was started.
pub const APP_VERIFICATION_FAILED: &str = "app_verification_failed";

/// Every `app.open` refusal code, in the order the daemon checks them.
pub const APP_OPEN_ERROR_CODES: [&str; 5] = [
    APP_OPEN_NOT_USED,
    APP_OPEN_DISABLED,
    APP_UNKNOWN,
    APP_NOT_INSTALLED,
    APP_VERIFICATION_FAILED,
];

/// Ask the daemon to start the operator app named `app`.
///
/// Starting the app grants nothing by itself.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppOpenRequest {
    op: AppOpenOp,
    /// The app's identifier in the daemon's compiled app table.
    pub app: String,
}

impl AppOpenRequest {
    pub fn new(app: impl Into<String>) -> Self {
        Self {
            op: AppOpenOp::Open,
            app: app.into(),
        }
    }
}

open_string_enum! {
    /// What an `app.open` call did.
    AppOpenStatus {
        /// The daemon started a new process.
        Launched => "launched",
        /// A process the daemon started earlier is still running. The daemon
        /// started nothing and did not bring the app to the foreground.
        AlreadyRunning => "already_running",
    }
}

/// A successful `app.open` reply.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppOpenReply {
    op: AppOpenOp,
    pub status: AppOpenStatus,
    /// The process id of the started or still-running app.
    pub pid: u32,
}

impl AppOpenReply {
    /// The daemon started a new process with this pid.
    pub fn launched(pid: u32) -> Self {
        Self {
            op: AppOpenOp::Open,
            status: AppOpenStatus::Launched,
            pid,
        }
    }

    /// The process the daemon started earlier, with this pid, is still running.
    pub fn already_running(pid: u32) -> Self {
        Self {
            op: AppOpenOp::Open,
            status: AppOpenStatus::AlreadyRunning,
            pid,
        }
    }
}

// A required, single-valued field both emits the op and refuses a missing or
// different op when decoding, without exposing a caller-editable tag.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum AppOpenOp {
    #[serde(rename = "app.open")]
    Open,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_open_bodies_require_the_op() {
        assert!(serde_json::from_value::<AppOpenRequest>(serde_json::json!({"app": "a"})).is_err());
        assert!(serde_json::from_value::<AppOpenRequest>(
            serde_json::json!({"op": "route.open", "app": "a"})
        )
        .is_err());
        assert!(serde_json::from_value::<AppOpenReply>(
            serde_json::json!({"status": "launched", "pid": 1})
        )
        .is_err());
    }

    #[test]
    fn an_app_open_status_from_a_newer_daemon_round_trips_as_unknown() {
        let wire = r#"{"op":"app.open","status":"focused","pid":7}"#;
        let reply: AppOpenReply = serde_json::from_str(wire).unwrap();
        assert_eq!(reply.status, AppOpenStatus::Unknown("focused".to_string()));
        assert_eq!(serde_json::to_string(&reply).unwrap(), wire);
    }
}
