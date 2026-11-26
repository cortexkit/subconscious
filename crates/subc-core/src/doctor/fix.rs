use crate::window::RevertOutcome;

use super::{run::Pass, DoctorHost, FixReport, FixSkip};

pub(super) fn attempt(
    host: &dyn DoctorHost,
    first: Pass,
    second: impl FnOnce() -> Pass,
) -> (Pass, FixReport, Option<u8>) {
    let mut fix = FixReport {
        result: None,
        skipped: None,
        findings_before: first.findings.clone(),
        module_fixes: Vec::new(),
    };
    let Some(offer) = &first.offer else {
        fix.skipped = Some(FixSkip::NothingToFix);
        return (first, fix, None);
    };
    if first.manager_overdue {
        fix.skipped = Some(FixSkip::ManagerObservationOverdue);
        return (first, fix, Some(1));
    }

    let result = host.revert(&offer.window_id);
    let outcome = result.outcome;
    fix.result = Some(result);
    match outcome {
        RevertOutcome::Reverted => (second(), fix, None),
        RevertOutcome::StoppedPartWay => (second(), fix, Some(1)),
        RevertOutcome::RefusedBeforeChange => (first, fix, Some(2)),
    }
}
