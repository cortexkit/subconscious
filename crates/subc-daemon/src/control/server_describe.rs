use super::*;

pub(super) const CAP_MANIFEST_REGISTRATION: &str = "manifest_registration_v1";
pub(super) const CAP_CHANNEL_LIFECYCLE: &str = "channel_lifecycle_v1";
pub(super) const CAP_PING_PONG: &str = "ping_pong_v1";
pub(super) const CAP_SESSION_ATTACH: &str = "session_attach_v1";
pub(super) const CAP_ADMISSION_FACTS_RELAY: &str = "admission_facts_relay_v1";

impl ControlHandler {
    pub(super) fn handle_server_describe(&self, frame: Frame) -> Result<Vec<Frame>, RouterError> {
        self.refresh_capability_requirements();
        // A bare connection count is ambiguous between many clients holding a
        // route each and one client accumulating hundreds, so publish the
        // concentration alongside it. Route state is best-effort here: a
        // diagnostic endpoint must still answer if the forwarding lock is
        // contended.
        let mut counters = self.counters.snapshot();
        if let Some(obj) = counters.as_object_mut() {
            obj.insert(
                "control_usage".into(),
                self.control_usage
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .snapshot(),
            );
        }
        let losses = self.resource_tracker.loss_diagnostics();
        if losses.as_object().is_some_and(|losses| !losses.is_empty()) {
            if let Some(obj) = counters.as_object_mut() {
                obj.insert("agent_command_declaration_loss_by_module".into(), losses);
            }
        }
        if let (Some(ledger), Some(obj)) = (&self.activity, counters.as_object_mut()) {
            obj.insert(
                "activity_unparseable_lines".into(),
                ledger.read_errors().into(),
            );
            obj.insert(
                "activity_lifecycle_append_failures".into(),
                ledger.lifecycle_failures().into(),
            );
            obj.insert(
                "activity_started_digest_skips".into(),
                ledger.lifecycle_digest_skips().into(),
            );
        }
        if let (Ok((connections_with_routes, max)), Some(obj)) = (
            self.forwarding.client_route_concentration(),
            counters.as_object_mut(),
        ) {
            obj.insert(
                "client_connections_with_routes".into(),
                connections_with_routes.into(),
            );
            obj.insert("max_routes_on_one_connection".into(), max.into());
        }
        // A module that is being fast-refused and a module that is fine look
        // identical from a client that retries and succeeds, so name the open
        // breakers here. This rides the existing free-form counters object
        // rather than a new wire field, so no sibling that deserializes
        // `ServerDescribe` has to be rebuilt to keep reading it.
        if let (Some(open_breakers), Some(obj)) = (
            self.route_bind_breakers.open_snapshot(),
            counters.as_object_mut(),
        ) {
            obj.insert("route_bind_breakers_open".into(), open_breakers);
        }
        // A daemon whose local endpoint could not be bound keeps serving TCP,
        // and the connection file names no endpoint. Name the reason here, in
        // the free-form counters object, so an operator reading daemon facts
        // sees it without searching the log, and no wire field changes.
        if let (Some(reason), Some(obj)) = (
            self.local_endpoint_unavailable.clone(),
            counters.as_object_mut(),
        ) {
            obj.insert("local_endpoint_unavailable".into(), reason.into());
        }
        let response = ClientControlResponse::ServerDescribe {
            protocol_ver: PROTOCOL_VERSION,
            subc_ops: subc_ops(),
            capabilities: self.describe_capabilities(),
            connected_clients: self.connected_clients.count(),
            counters: Some(counters),
            build_git_sha: Some(env!("SUBC_BUILD_GIT_SHA").to_string()),
            build_lock_digest: Some(env!("SUBC_BUILD_LOCK_DIGEST").to_string()),
            capability_requirements: self.capability_requirement_statuses(),
            machine_id: self.machine_id.as_ref().map(|id| id.as_str().to_owned()),
            config_path: self
                .rescan
                .as_ref()
                .and_then(|context| rescan_checked::reported_config_path(&context.config_path)),
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::ServerDescribe",
        )?])
    }
}

pub(super) fn subc_ops() -> Vec<String> {
    SUBC_CONTROL_OPS
        .iter()
        .map(|op| (*op).to_string())
        .collect()
}
