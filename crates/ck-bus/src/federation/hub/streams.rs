//! The hub streams' JetStream configurations, from `cortexkit-bus-naming`'s descriptors.

use async_nats::jetstream::stream;
use cortexkit_bus_naming::federation::{
    cuts_object_store, hub_idlog_stream, hub_inbox_stream, hub_memory_stream, hub_rooms_stream,
    residence_bucket, vault_manifest_active_bucket, vault_shared_logins_bucket, BoxId, HubKind,
    HubStreamSpec,
};

use super::seams::HubStream;
use crate::bootstrap::plane::stream_config;

/// The stream to create for `hub_stream`, or a refusal for a machine id in the wrong
/// spelling.
pub fn config_for(hub_stream: &HubStream) -> Result<stream::Config, String> {
    let spec = match hub_stream {
        HubStream::Inbox { box_id, hub } => hub_inbox_stream(
            &BoxId::parse(box_id).map_err(|error| format!("{box_id:?}: {error}"))?,
            hub,
        ),
        HubStream::IdLog => hub_idlog_stream(),
        HubStream::Residence => residence_bucket(HubKind::SelfHosted).backing_stream,
        HubStream::Cuts => cuts_object_store(),
        HubStream::Rooms => hub_rooms_stream(),
        HubStream::Memory => hub_memory_stream(),
        HubStream::VaultSharedLogins => {
            vault_shared_logins_bucket(HubKind::SelfHosted).backing_stream
        }
        HubStream::VaultManifestActive => {
            vault_manifest_active_bucket(HubKind::SelfHosted).backing_stream
        }
    };
    Ok(hub_config(&spec))
}

/// The local stream mapping plus the settings only hub streams carry.
fn hub_config(spec: &HubStreamSpec) -> stream::Config {
    let mut config = stream_config(&spec.stream);
    if let Some(replicas) = spec.replicas {
        config.num_replicas = replicas;
    }
    config.allow_rollup = spec.allow_rollup_hdrs;
    config.allow_direct = spec.allow_direct;
    config.deny_delete = spec.deny_delete;
    config
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn hub_configs_use_naming_consumer_caps() {
        let box_id = "0123456789abcdef0123456789abcdef";
        let hub =
            cortexkit_bus_naming::federation::HubName::from_key_pin(&"0f1e2d3c4b5a6978".repeat(4))
                .unwrap();
        for (stream, spec) in [
            (
                HubStream::Inbox {
                    box_id: box_id.to_owned(),
                    hub: hub.clone(),
                },
                hub_inbox_stream(&BoxId::parse(box_id).unwrap(), &hub),
            ),
            (HubStream::IdLog, hub_idlog_stream()),
            (
                HubStream::Residence,
                residence_bucket(HubKind::SelfHosted).backing_stream,
            ),
            (HubStream::Cuts, cuts_object_store()),
            (HubStream::Rooms, hub_rooms_stream()),
            (HubStream::Memory, hub_memory_stream()),
            (
                HubStream::VaultSharedLogins,
                vault_shared_logins_bucket(HubKind::SelfHosted).backing_stream,
            ),
            (
                HubStream::VaultManifestActive,
                vault_manifest_active_bucket(HubKind::SelfHosted).backing_stream,
            ),
        ] {
            let config = config_for(&stream).unwrap();
            assert_eq!(
                Some(config.max_consumers),
                spec.stream.max_consumers,
                "{}",
                config.name
            );
        }
    }

    #[test]
    fn hub_streams_follow_the_naming_crate() {
        let hub =
            cortexkit_bus_naming::federation::HubName::from_key_pin(&"0f1e2d3c4b5a6978".repeat(4))
                .unwrap();
        let inbox = config_for(&HubStream::Inbox {
            box_id: "0123456789abcdef0123456789abcdef".to_string(),
            hub: hub.clone(),
        })
        .unwrap();
        assert_eq!(
            inbox.name,
            "HUB_INBOX_0123456789abcdef0123456789abcdef_0f1e2d3c4b5a6978"
        );
        assert_eq!(inbox.discard, stream::DiscardPolicy::New);
        assert_eq!(inbox.max_age, Duration::from_secs(7 * 24 * 3600));
        assert!(config_for(&HubStream::Inbox {
            box_id: "0123456789ABCDEF0123456789ABCDEF".to_string(),
            hub,
        })
        .is_err());

        let idlog = config_for(&HubStream::IdLog).unwrap();
        assert_eq!(idlog.subjects, ["ck.fleet.idlog"]);
        assert_eq!(idlog.max_age, Duration::ZERO);

        let residence = config_for(&HubStream::Residence).unwrap();
        assert_eq!(residence.name, "KV_residence");
        assert_eq!(residence.num_replicas, 1);
        assert_eq!(residence.max_messages_per_subject, 1);
        assert!(residence.allow_rollup && residence.allow_direct && residence.deny_delete);

        let cuts = config_for(&HubStream::Cuts).unwrap();
        assert_eq!(cuts.name, "OBJ_cuts");
        assert!(cuts.allow_rollup);
        assert_eq!(cuts.discard, stream::DiscardPolicy::New);

        let rooms = config_for(&HubStream::Rooms).unwrap();
        assert_eq!(rooms.name, "HUB_ROOMS");
        assert_eq!(rooms.max_messages_per_subject, 100_000);
        assert_eq!(rooms.max_message_size, 256 * 1024);
        assert_eq!(rooms.max_age, Duration::ZERO);

        let memory = config_for(&HubStream::Memory).unwrap();
        assert_eq!(memory.name, "HUB_MEMORY");
        assert_eq!(memory.subjects, ["ck.fleet.memory.*.*"]);
        assert_eq!(memory.max_message_size, 64 * 1024);
        assert_eq!(memory.discard, stream::DiscardPolicy::New);
    }
}
