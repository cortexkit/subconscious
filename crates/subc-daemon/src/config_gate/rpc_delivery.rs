use super::*;

impl RuntimeOwner {
    pub(crate) fn module_get_for_rpc(
        &self,
        connection: ConnectionId,
        module: &str,
        request: ConfigGetRequest,
    ) -> Result<ConfigGetReply> {
        let mut state = self.lock()?;
        let witness = |state: &OwnerState| {
            (
                state
                    .delivery
                    .attempts
                    .get(&connection)
                    .map(|bound| bound.id),
                state.delivery.last_target_attempt.get(module).copied(),
                store_witness(&state.store),
            )
        };
        let before = witness(&state);
        let reply = self.module_get_locked(&mut state, connection, module, request, true);
        if witness(&state) != before {
            self.notify_delivery_changed();
        }
        reply
    }

    pub(super) fn record_rpc_write(
        &self,
        connection: ConnectionId,
        generation: u64,
        attempt: u64,
        written_at: tokio::time::Instant,
    ) -> Result<()> {
        if self.record_delivery_written(connection, generation, attempt, written_at)? {
            self.notify_delivery_changed();
        }
        Ok(())
    }
}

pub(super) fn record_served_view(
    state: &mut OwnerGuard<'_>,
    connection: ConnectionId,
    reply: &ConfigGetReply,
) -> bool {
    let vector = (
        reply.active.as_ref().map(|view| view.generation),
        reply.pending.as_ref().map(|view| view.generation),
    );
    let vector_changed = state.delivery.served.get(&connection) != Some(&vector);
    if vector_changed {
        state.delivery.served.insert(connection, vector);
    }
    let unstamped = reply
        .active
        .as_ref()
        .filter(|view| view.status == ConfigStatus::Unstamped);
    let view_changed = state
        .delivery
        .unstamped_views
        .get(&connection)
        .map(AsRef::as_ref)
        != unstamped;
    if view_changed {
        if let Some(view) = unstamped {
            state
                .delivery
                .unstamped_views
                .insert(connection, std::sync::Arc::new(view.clone()));
        } else {
            state.delivery.unstamped_views.remove(&connection);
        }
    }
    vector_changed || view_changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_gate::runtime::settings::tests::SettingsRig;

    #[tokio::test]
    async fn repeated_module_get_does_not_wake_delivery_until_the_view_changes() {
        let rig = SettingsRig::new();
        let connection = ConnectionId::new(10);
        let module = "sample-reader";
        let notifications = rig.owner.delivery_notifications();
        let woke = || {
            let event = notifications.notified();
            tokio::pin!(event);
            event.as_mut().enable()
        };
        woke(); // Drain the wake from the fixture declaring sample-reader.
        let get = || {
            rig.owner
                .module_get_for_rpc(connection, module, ConfigGetRequest::new())
                .unwrap()
        };
        let written = |view: &ConfigGetReply| {
            rig.owner
                .module_view_delivered(connection, module, view, tokio::time::Instant::now(), true)
                .unwrap();
        };
        let first = get();
        assert!(!woke());
        written(&first);
        assert!(woke(), "first successful receipt changes the served view");
        written(&get());
        assert!(!woke(), "the repeated view and receipt changed nothing");
        rig.write(br#"{"limit":99,"credential":{"vault_ref":"sample-reference"}}"#);
        written(&get());
        assert!(
            woke(),
            "a changed unstamped view must be recorded and signalled"
        );
    }
}
