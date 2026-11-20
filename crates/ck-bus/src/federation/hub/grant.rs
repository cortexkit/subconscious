//! The leaf user grant a hub-issued JWT carries, from `cortexkit-bus-naming`.

use cortexkit_bus_naming::federation::{leaf_user_grant, BoxId, HubName};

/// Include the previous names only while that box's streams are draining on this hub.
pub fn leaf_permissions(
    box_id: &BoxId,
    hub: &HubName,
    draining: &[HubName],
) -> (Vec<String>, Vec<String>) {
    let grant = leaf_user_grant(box_id, hub);
    let mut publish = grant.publish;
    for name in draining {
        for subject in leaf_user_grant(box_id, name).publish {
            if !publish.contains(&subject) {
                publish.push(subject);
            }
        }
    }
    (publish, grant.subscribe)
}
