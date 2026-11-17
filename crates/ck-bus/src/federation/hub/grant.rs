//! The leaf user grant a hub-issued JWT carries, from `cortexkit-bus-naming`.

use cortexkit_bus_naming::federation::{leaf_user_grant, BoxId};

/// The publish and subscribe allow lists for `{box}`'s leaf user, in the naming crate's
/// order, which every hub issuer shares. A machine id in any spelling other than 32
/// lowercase hex is refused.
pub fn leaf_permissions(box_id: &str) -> Result<(Vec<String>, Vec<String>), String> {
    let box_id = BoxId::parse(box_id).map_err(|error| format!("{box_id:?}: {error}"))?;
    let grant = leaf_user_grant(&box_id);
    Ok((grant.publish, grant.subscribe))
}
