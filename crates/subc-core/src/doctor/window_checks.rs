use serde_json::json;

use crate::window::journal::{Action, Liveness, Snapshot, State};

use super::report::{Finding, Layer, Offer, Severity};

pub(crate) type WindowRow = (Result<Snapshot, (String, String)>, Option<Liveness>);

#[derive(Default)]
pub(super) struct WindowFindings {
    pub(super) findings: Vec<Finding>,
    pub(super) offer: Option<Offer>,
    pub(super) blocker: Option<(&'static str, String)>,
}

pub(super) fn classify(rows: Vec<WindowRow>, now_ms: u64) -> WindowFindings {
    let mut result = WindowFindings::default();
    let offer_allowed = rows.iter().all(|(row, _)| row.is_ok());
    let mut seen_readable = false;
    for (row, liveness) in rows {
        let snapshot = match row {
            Ok(snapshot) => snapshot,
            Err((id, error)) => {
                let finding_id = "window.unreadable";
                result
                    .blocker
                    .get_or_insert_with(|| (finding_id, id.clone()));
                result.findings.push(
                    Finding::new(
                        finding_id,
                        Layer::Window,
                        Severity::Error,
                        format!("Window {id} is unreadable; recover this window by hand"),
                        json!({"window_id": id, "error": error}),
                    )
                    .for_subject(id),
                );
                continue;
            }
        };
        let newest = !std::mem::replace(&mut seen_readable, true);
        let state = snapshot.state(|_| liveness.unwrap_or(Liveness::Unreadable));
        if !newest && !matches!(state, State::Reverting | State::RevertInterrupted) {
            continue;
        }
        let unfinished = snapshot.unfinished().map(|(index, _)| index);
        let count = snapshot.steps.len();
        let (id, severity, summary, offer) = match state {
            State::Interrupted => (
                "window.interrupted",
                Severity::Error,
                format!(
                    "The update stopped at step {} of {count}",
                    unfinished.expect("an interrupted window has an unfinished step")
                ),
                true,
            ),
            State::RevertInterrupted => (
                "window.revert_interrupted",
                Severity::Error,
                "The revert stopped part way; rerunning it resumes".into(),
                true,
            ),
            State::Reverting if liveness == Some(Liveness::Alive) => (
                "window.reverting",
                Severity::Info,
                "A revert is running".into(),
                false,
            ),
            State::Reverting => (
                "window.reverting",
                Severity::Warning,
                "A revert may be running; its owner can't be confirmed".into(),
                false,
            ),
            State::Open if unfinished.is_some() => (
                "window.update_in_progress",
                Severity::Info,
                "An update is in progress".into(),
                false,
            ),
            State::Open if count == 0 => (
                "window.empty",
                Severity::Info,
                "An empty window is open; run `ck window keep`".into(),
                false,
            ),
            State::Open if now_ms.saturating_sub(snapshot.began_ms) < 24 * 60 * 60 * 1000 => (
                "window.not_kept",
                Severity::Warning,
                "An update changed things in the last 24 h; keep it or revert it".into(),
                true,
            ),
            State::Open => (
                "window.not_kept",
                Severity::Info,
                "Run `ck window keep`".into(),
                false,
            ),
            _ => continue,
        };

        if matches!(state, State::Reverting | State::RevertInterrupted) {
            result
                .blocker
                .get_or_insert_with(|| (id, snapshot.id.clone()));
        }
        if offer && newest && offer_allowed {
            result.offer = Some(Offer {
                command: format!("ck window revert {}", snapshot.id),
                window_id: snapshot.id.clone(),
            });
        }
        let steps: Vec<_> = snapshot
            .steps
            .iter()
            .map(|entry| {
                let action = match entry.step.action {
                    Action::File { .. } => "file",
                    Action::Bundle { .. } => "bundle",
                    Action::Store { .. } => "store",
                    Action::Activation { .. } => "activation",
                };
                json!({"action": action, "target": entry.step.target.to_string_lossy()})
            })
            .collect();
        let liveness = liveness.map(|liveness| match liveness {
            Liveness::Alive => "alive",
            Liveness::Gone => "gone",
            Liveness::Unreadable => "unreadable",
        });
        let state = state.to_string().replace('-', "_");
        result.findings.push(
            Finding::new(
                id,
                Layer::Window,
                severity,
                summary,
                json!({
                    "window_id": snapshot.id, "state": state, "began_ms": snapshot.began_ms,
                    "step_count": count, "unfinished_index": unfinished, "steps": steps,
                    "owner_liveness": liveness,
                }),
            )
            .for_subject(snapshot.id),
        );
    }
    result
}
