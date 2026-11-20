use super::*;

pub(super) fn observed_exit(module_id: &str, status: io::Result<ExitStatus>) -> ExitReport {
    match status {
        Ok(status) => classify_exit(&status),
        Err(error) => {
            warn!(module_id, %error, "could not reap the abandoned swap candidate");
            wait_error_exit_report()
        }
    }
}

pub(super) async fn abandoned_exit(
    module_id: &str,
    candidate: &mut SupervisedChild,
    failure: Option<&CandidateFailure>,
) -> ExitReport {
    if let Some(exit) = failure.and_then(|failure| failure.exit.as_ref()) {
        return exit.clone();
    }
    if let Err(error) = candidate.start_kill() {
        debug!(module_id, %error, "swap candidate kill failed; it may already have exited");
    }
    observed_exit(module_id, candidate.wait().await)
}

pub(super) fn record_abandonment(
    module_id: &str,
    runtime: &SupervisorRuntimeConfig,
    exit: &ExitReport,
    failure: Option<&CandidateFailure>,
) {
    let reason = match failure {
        Some(failure) => format!(
            "swap candidate {}: {}",
            failure.arm.as_str(),
            failure.detail
        ),
        None => format!(
            "swap candidate {}",
            SwapOutcome::SupersededByConfigRestart.as_str()
        ),
    };
    // The candidate never became the active spawn. Record its exit without
    // publishing a spawn event or making an active lifecycle decision.
    runtime
        .terminal_ring
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .record_exit(
            module_id,
            TerminalRecord {
                exit_code: exit.code,
                exit_signal: exit.signal,
                at_ms: exit.at_ms,
                disposition: TerminalDisposition::Stopped,
                exit_kind: exit.kind.into(),
                disposition_detail: Some(reason),
            },
        );
}
