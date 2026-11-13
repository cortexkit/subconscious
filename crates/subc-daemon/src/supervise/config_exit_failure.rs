use super::*;

/// Why a configuration-exit fence remains closed after bounded reconciliation.
/// Codes and error kinds are diagnostic names, never raw store or program data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleStopReason {
    pub code: String,
    pub error_kind: String,
}

impl SupervisedModule {
    pub(crate) fn config_exit_failed(
        &self,
        reason: ModuleStopReason,
    ) -> Result<(), SuperviseError> {
        update_snapshot(&self.inner.snapshot, Some(self.module_id()), |state| {
            state.state = ModuleState::Stopped;
            state.respawn_pending = false;
            state.failed_retry.pending = None;
            state.config_hold_parked = false;
            state.config_hold_daemon_initiated = false;
            state.config_hold_approval = None;
            clear_current_process_facts(state);
            state.stop_reason = Some(reason);
        })
    }
}
