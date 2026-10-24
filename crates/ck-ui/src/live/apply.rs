//! Apply the reviewed candidate, then follow only its restarts, in review order.

use super::settings::{fleet_drain_response_timeout, ApplyOutcome, ApplyReply};
use futures_util::future::BoxFuture;
use std::time::Duration;
use subc_control::{RescanExpect, SupervisorEntry};

pub trait Service: Sync {
    fn rescan(&self, expect: RescanExpect, timeout: Duration) -> BoxFuture<'_, ApplyOutcome>;
    fn acknowledged(&self, reply: ApplyReply);
    fn restart(&self, module: String, notes: String) -> BoxFuture<'_, RestartResult>;
    fn result(&self, ok: bool, text: String);
}

pub struct RestartResult {
    pub healthy: bool,
    pub state: String,
    pub detail: String,
}

/// Progress must name its module even after the sequence advances beyond the
/// selected row. Keep the rescan's diagnostics visible while a drain runs.
pub fn progress_line(module: &str, action: &crate::model::ModuleAction, notes: &str) -> String {
    let line = format!("{module} {} · {}", action.kind.verb(), action.phase_text());
    if notes.is_empty() {
        line
    } else {
        format!("{line} · {notes}")
    }
}

pub async fn run(
    service: &impl Service,
    revision: u64,
    expect: RescanExpect,
    restarts: Vec<String>,
    roster: &[SupervisorEntry],
) {
    let outcome = service
        .rescan(expect, fleet_drain_response_timeout(roster))
        .await;
    service.acknowledged(ApplyReply {
        revision,
        restarts: restarts.clone(),
        outcome: outcome.clone(),
    });
    let ApplyOutcome::Ready(result) = outcome else {
        return;
    };
    let mut notes: Vec<String> = result
        .restart_required
        .iter()
        .map(|section| format!("{section} changed; it applies only when the daemon restarts"))
        .chain(result.capability_warnings.iter().cloned())
        .collect();
    let extra: Vec<_> = result
        .module_actions
        .iter()
        .filter(|a| a.action == subc_control::RescanModuleActionKind::Restart)
        .map(|a| &a.module_id)
        .chain(result.changed_pending_reload.iter())
        .filter(|id| !restarts.contains(id))
        .cloned()
        .collect();
    if !extra.is_empty() {
        let extra: std::collections::BTreeSet<_> = extra.into_iter().collect();
        notes.push(format!(
            "Not restarted (not in the preview): {}",
            extra.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    let note = notes.join(" · ");
    let with_note = |text: String| {
        if note.is_empty() {
            text
        } else {
            format!("{text} · {note}")
        }
    };
    service.result(true, with_note("Saved and applied".into()));
    for (index, module) in restarts.iter().enumerate() {
        let restart = service.restart(module.clone(), note.clone()).await;
        if !restart.healthy {
            service.result(
                false,
                with_note(format!(
                    "✗ {module} {} · not restarted: {} · {}",
                    restart.state,
                    restarts[index + 1..].join(", "),
                    restart.detail
                )),
            );
            return;
        }
        service.result(true, with_note(restart.detail));
    }
}
