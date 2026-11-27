//! The words `ck module list` and `ck module status` show for a module's run
//! state: why it isn't running, when it retries, or that it waits for an
//! earlier instance to exit.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use super::{format_exit, inline_views, terminal_safe_string};

pub(super) fn enabled_drift_marker(module: &Value) -> &'static str {
    match (
        module.get("enabled").and_then(Value::as_bool),
        module.get("configured_enabled").and_then(Value::as_bool),
    ) {
        (Some(true), Some(false)) => " (config: disabled)",
        (Some(false), Some(true)) => " (config: enabled)",
        _ => "",
    }
}

pub(super) fn runtime_status_text(module: &Value) -> String {
    if let Some(detail) = module
        .get("waiting_for_previous_detail")
        .and_then(Value::as_str)
    {
        return terminal_safe_string(detail);
    }
    if let Some(pid) = module
        .get("waiting_for_previous_pid")
        .and_then(Value::as_u64)
    {
        return format!("waiting: a previous instance (pid {pid}) may still be running; it will start once that process exits");
    }
    if module.get("enabled").and_then(Value::as_bool) == Some(false) {
        return "stopped".to_string();
    }
    let state = module
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    match state {
        "disabled" | "stopped" => "stopped".to_string(),
        "spawning" | "starting" => "starting".to_string(),
        "restarting" => "restarting".to_string(),
        "failed" => {
            if let Some(retry_at) = module.get("next_retry_at_ms").and_then(Value::as_u64) {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_millis() as u64)
                    .unwrap_or(0);
                let minutes = retry_at.saturating_sub(now).div_ceil(60_000);
                return format!("failed, retrying in {minutes}m");
            }
            let mut detail = Vec::new();
            if let Some(exit) = format_exit(
                module.get("last_exit_signal").and_then(Value::as_i64),
                module.get("last_exit_code").and_then(Value::as_i64),
            ) {
                detail.push(exit);
            }
            if let Some(restarts) = module.get("restart_count").and_then(Value::as_u64) {
                let noun = if restarts == 1 { "restart" } else { "restarts" };
                detail.push(format!("{restarts} {noun}"));
            }
            if detail.is_empty() {
                "failed".to_string()
            } else {
                format!("failed ({})", detail.join(", "))
            }
        }
        "running" if module.get("live").and_then(Value::as_bool) == Some(false) => {
            "starting".to_string()
        }
        other => inline_views::runtime_state_label(module, other),
    }
}
