//! Callosum's view of the user's paired devices, as ck-bus reads it.
//!
//! `callosum.paired_devices_read` and each `callosum.paired_devices_watch` notice carry
//! `{devices: [{fingerprint, class}], self, tombstones: [{fingerprint, removal_id}]}` for
//! `reserved:ckbus`. `callosum.peer_keys_read` carries `{peers: [{fingerprint,
//! machine_id?, ...}]}`; callosum bound the machine id at pairing, and it is the only
//! source of a computer's `{box}`.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

/// One snapshot of the paired devices.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Devices {
    /// Fingerprint to class (`computer`, `phone`, `sim`, `unknown`).
    pub listed: BTreeMap<String, String>,
    pub tombstoned: BTreeSet<String>,
}

impl Devices {
    /// A computer that may hold a hub leaf credential: listed, a computer, and named by
    /// no tombstone.
    pub fn is_live_computer(&self, fingerprint: &str) -> bool {
        !self.tombstoned.contains(fingerprint)
            && self.listed.get(fingerprint).map(String::as_str) == Some("computer")
    }

    /// A computer that has left: named by a tombstone, or no longer listed (which is
    /// how a local unpair shows).
    pub fn is_removed(&self, fingerprint: &str) -> bool {
        self.tombstoned.contains(fingerprint) || !self.listed.contains_key(fingerprint)
    }
}

/// Parses one read reply or watch notice. A `{"result": ...}` envelope is unwrapped
/// first, so either framing reads the same. Anything not of this shape is refused
/// whole: a partial list would read as removals.
pub fn parse_devices(body: &[u8]) -> Result<Devices, String> {
    let value = unwrap(serde_json::from_slice(body).map_err(|e| format!("not JSON: {e}"))?);
    let devices = value["devices"]
        .as_array()
        .ok_or("the paired devices answer has no devices list")?;
    let tombstones = value["tombstones"]
        .as_array()
        .ok_or("the paired devices answer has no tombstones list")?;
    let mut snapshot = Devices::default();
    for device in devices {
        let (Some(fingerprint), Some(class)) =
            (device["fingerprint"].as_str(), device["class"].as_str())
        else {
            return Err(format!(
                "a device entry lacks fingerprint or class: {device}"
            ));
        };
        snapshot
            .listed
            .insert(fingerprint.to_string(), class.to_string());
    }
    for tombstone in tombstones {
        let fingerprint = tombstone["fingerprint"]
            .as_str()
            .ok_or_else(|| format!("a tombstone lacks its fingerprint: {tombstone}"))?;
        snapshot.tombstoned.insert(fingerprint.to_string());
    }
    Ok(snapshot)
}

/// The machine id callosum bound to `fingerprint` at pairing, if it lists one.
pub fn machine_id_for(body: &[u8], fingerprint: &str) -> Result<Option<String>, String> {
    let value = unwrap(serde_json::from_slice(body).map_err(|e| format!("not JSON: {e}"))?);
    let peers = value["peers"]
        .as_array()
        .ok_or("the peer keys answer has no peers list")?;
    Ok(peers
        .iter()
        .find(|peer| peer["fingerprint"].as_str() == Some(fingerprint))
        .and_then(|peer| peer["machine_id"].as_str())
        .map(str::to_string))
}

fn unwrap(value: Value) -> Value {
    match value {
        Value::Object(mut object) if object.len() == 1 && object.contains_key("result") => {
            object.remove("result").unwrap_or(Value::Null)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `callosum/test-vectors/paired-devices-v3/cases.json`, case `ckbus_read`.
    const CKBUS_READ: &str = r#"{"devices":[{"class":"computer","fingerprint":"648aa5c579fb30f38af744d97d6ec840"},{"class":"unknown","fingerprint":"9f4fb68f3e1dac82202f9aa581ce0bbf"}],"self":{"static_public_key_hex":"8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a"},"tombstones":[{"fingerprint":"72cd6e8422c407fb6d098690f1130b7d","removal_id":"1000:permanent:2"},{"fingerprint":"75877bb41d393b5fb8455ce60ecd8dda","removal_id":"2000:5:3"},{"fingerprint":"f849d67325facf04177bc663b2dc5440","removal_id":"3000:7:4"}]}"#;
    // `callosum/test-vectors/peer-keys-v3/cases.json`, case
    // `ckbus_sees_bus_and_verification_keys`, the reply.
    const CKBUS_PEER_KEYS: &str = r#"{"peers":[{"fingerprint":"72cd6e8422c407fb6d098690f1130b7d","key_record":{"keys":{"bus_seal_x25519":{"pub":"ff06","version":"17"},"fleet_record_ed25519":{"pub":"ee05","version":"13"},"msgsig_ed25519":{"pub":"bb02","version":"5"}}},"machine_id":"853ecdb6e1681c3dc8c1c578d14c23a2","registry_platform":"ios"}]}"#;

    #[test]
    fn reads_callosums_published_ckbus_samples() {
        let devices = parse_devices(CKBUS_READ.as_bytes()).unwrap();
        assert!(devices.is_live_computer("648aa5c579fb30f38af744d97d6ec840"));
        // Listed but not a computer.
        assert!(!devices.is_live_computer("9f4fb68f3e1dac82202f9aa581ce0bbf"));
        assert!(!devices.is_removed("9f4fb68f3e1dac82202f9aa581ce0bbf"));
        // Tombstoned, and never listed.
        assert!(devices.is_removed("72cd6e8422c407fb6d098690f1130b7d"));
        assert!(!devices.is_live_computer("72cd6e8422c407fb6d098690f1130b7d"));
        assert!(devices.is_removed("00000000000000000000000000000000"));
        // A watch notice in the envelope reads the same.
        let wrapped = format!(r#"{{"result":{CKBUS_READ}}}"#);
        assert_eq!(parse_devices(wrapped.as_bytes()).unwrap(), devices);

        assert_eq!(
            machine_id_for(
                CKBUS_PEER_KEYS.as_bytes(),
                "72cd6e8422c407fb6d098690f1130b7d"
            )
            .unwrap()
            .as_deref(),
            Some("853ecdb6e1681c3dc8c1c578d14c23a2")
        );
        assert_eq!(
            machine_id_for(CKBUS_PEER_KEYS.as_bytes(), &"0".repeat(32)).unwrap(),
            None
        );
    }

    #[test]
    fn a_malformed_snapshot_is_refused_whole() {
        assert!(parse_devices(br#"{"devices":[]}"#).is_err());
        assert!(parse_devices(br#"{"devices":[{"class":"computer"}],"tombstones":[]}"#).is_err());
        assert!(parse_devices(b"not json").is_err());
    }
}
