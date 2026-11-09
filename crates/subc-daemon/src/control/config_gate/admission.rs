use super::*;

pub(super) struct SupervisedReviewAdmission(pub(super) ControlHandler);

impl crate::config_gate::ReviewAdmission for SupervisedReviewAdmission {
    fn check(
        &self,
        registration: &crate::registry::ModuleRegistration,
    ) -> std::result::Result<(), GateError> {
        let module = &registration.manifest.module_id;
        let current = self
            .0
            .registry
            .get_module(module)
            .ok()
            .flatten()
            .is_some_and(|current| current.connection_id == registration.connection_id)
            && self
                .0
                .forwarding
                .active_module_endpoint(module)
                .ok()
                .flatten()
                .is_some_and(|endpoint| endpoint.connection_id == registration.connection_id);
        let nonce = self
            .0
            .hello_launch_nonces
            .lock()
            .map_err(|_| GateError::new("program_refused", "HELLO provenance is unavailable"))?
            .nonce(registration.connection_id)
            .map(str::to_owned);
        if !current {
            return Err(GateError::new(
                "program_refused",
                "reviewer is not the active delivery target",
            ));
        }
        self.0
            .supervisor
            .check_program_review(module, nonce.as_deref().unwrap_or_default())
    }
}
