//! The hub record, `hub.json` in ck-bus's store: what hub setup produced and what the hub
//! has issued and revoked since. It holds public keys, key ids, the listener
//! certificate and issue records only, never key material.
//!
//! The certificate's DER is kept here so every restart serves the same bytes; it is
//! rebuilt only when the TLS key rotates. Issue records are keyed on the computer's
//! fingerprint, because a machine id is a name the computer reports, never an authority.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Map, Value};

use crate::bootstrap::store::{read_json, write_atomic};

pub const HUB_RECORD_FILE: &str = "hub.json";

/// One computer's leaf credential as last issued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issued {
    /// The computer's machine id, the `{box}` its grant names.
    pub box_id: String,
    pub leaf_nkey: String,
    pub issued_at: i64,
    /// The last issue time of each earlier key whose JWT may still be live.
    pub previous_keys: BTreeMap<String, i64>,
}

/// A computer the hub stopped serving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    pub box_id: String,
    pub leaf_nkey: String,
    pub removed_at: i64,
}

/// What hub setup produced, and the issue and revocation state since.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubRecord {
    /// The hub operator (`O...`): the vault's `signing:bus-hub-operator:1`.
    pub operator_public: String,
    /// The vault `key_id` of each hub key when setup last ran; a different current
    /// one means the operator rotated that key and setup runs again.
    pub operator_key_id: String,
    pub account_key_id: String,
    pub tls_key_id: String,
    /// The hub system account and the user's hub account (`A...`).
    pub system_account: String,
    pub hub_account: String,
    /// The listener certificate, DER.
    pub cert_der: Vec<u8>,
    pub spki_sha256: String,
    pub issued: BTreeMap<String, Issued>,
    pub removed: BTreeMap<String, Removed>,
    /// Leaf keys revoked in the hub account JWT, each with its cutoff.
    pub revocations: BTreeMap<String, i64>,
    /// False while a revocation has been recorded here but not yet read back from the
    /// hub server.
    pub revocations_pushed: bool,
}

pub enum RecordFile {
    Absent,
    Present(Box<HubRecord>),
    Damaged { path: PathBuf, reason: String },
}

pub fn record_path(store_root: &Path) -> PathBuf {
    store_root.join(HUB_RECORD_FILE)
}

pub fn read(store_root: &Path) -> RecordFile {
    let path = record_path(store_root);
    match read_json(&path) {
        Ok(None) => RecordFile::Absent,
        Ok(Some(value)) => match parse(&value) {
            Ok(record) => RecordFile::Present(Box::new(record)),
            Err(reason) => RecordFile::Damaged { path, reason },
        },
        Err(reason) => RecordFile::Damaged { path, reason },
    }
}

pub fn write(store_root: &Path, record: &HubRecord) -> Result<(), String> {
    let path = record_path(store_root);
    write_atomic(&path, &to_json(record)).map_err(|error| format!("{}: {error}", path.display()))
}

pub fn to_json(record: &HubRecord) -> Value {
    let issued: Map<String, Value> = record
        .issued
        .iter()
        .map(|(fingerprint, issued)| {
            (
                fingerprint.clone(),
                json!({
                    "box": issued.box_id,
                    "leaf_nkey": issued.leaf_nkey,
                    "issued_at": issued.issued_at,
                    "previous_keys": issued.previous_keys,
                }),
            )
        })
        .collect();
    let removed: Map<String, Value> = record
        .removed
        .iter()
        .map(|(fingerprint, removed)| {
            (
                fingerprint.clone(),
                json!({
                    "box": removed.box_id,
                    "leaf_nkey": removed.leaf_nkey,
                    "removed_at": removed.removed_at,
                }),
            )
        })
        .collect();
    json!({
        "operator_public": record.operator_public,
        "operator_key_id": record.operator_key_id,
        "account_key_id": record.account_key_id,
        "tls_key_id": record.tls_key_id,
        "system_account": record.system_account,
        "hub_account": record.hub_account,
        "cert_der_b64": URL_SAFE_NO_PAD.encode(&record.cert_der),
        "spki_sha256": record.spki_sha256,
        "issued": issued,
        "removed": removed,
        "revocations": record.revocations,
        "revocations_pushed": record.revocations_pushed,
    })
}

pub fn parse(value: &Value) -> Result<HubRecord, String> {
    let text = |name: &str| {
        value[name]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("{name} is missing or not a string"))
    };
    let entries = |name: &str| -> Result<Map<String, Value>, String> {
        value[name]
            .as_object()
            .cloned()
            .ok_or_else(|| format!("{name} is not an object"))
    };
    let field = |entry: &Value, name: &str, what: &str| {
        entry[name]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("{what}: {name} is missing"))
    };
    let mut issued = BTreeMap::new();
    for (fingerprint, entry) in entries("issued")? {
        issued.insert(
            fingerprint.clone(),
            Issued {
                box_id: field(&entry, "box", &fingerprint)?,
                leaf_nkey: field(&entry, "leaf_nkey", &fingerprint)?,
                issued_at: entry["issued_at"].as_i64().unwrap_or_default(),
                previous_keys: entry
                    .get("previous_keys")
                    .map(|keys| {
                        serde_json::from_value(keys.clone())
                            .map_err(|error| format!("{fingerprint}: previous_keys: {error}"))
                    })
                    .transpose()?
                    .unwrap_or_default(),
            },
        );
    }
    let mut removed = BTreeMap::new();
    for (fingerprint, entry) in entries("removed")? {
        removed.insert(
            fingerprint.clone(),
            Removed {
                box_id: field(&entry, "box", &fingerprint)?,
                leaf_nkey: field(&entry, "leaf_nkey", &fingerprint)?,
                removed_at: entry["removed_at"].as_i64().unwrap_or_default(),
            },
        );
    }
    let revocations = entries("revocations")?
        .into_iter()
        .map(|(key, at)| {
            at.as_i64()
                .map(|at| (key.clone(), at))
                .ok_or_else(|| format!("revocation of {key} has no time"))
        })
        .collect::<Result<_, _>>()?;
    Ok(HubRecord {
        operator_public: text("operator_public")?,
        operator_key_id: text("operator_key_id")?,
        account_key_id: text("account_key_id")?,
        tls_key_id: text("tls_key_id")?,
        system_account: text("system_account")?,
        hub_account: text("hub_account")?,
        cert_der: URL_SAFE_NO_PAD
            .decode(text("cert_der_b64")?)
            .map_err(|_| "cert_der_b64 is not base64url".to_string())?,
        spki_sha256: text("spki_sha256")?,
        issued,
        removed,
        revocations,
        revocations_pushed: value["revocations_pushed"].as_bool().unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = HubRecord {
            operator_public: "OOP".into(),
            operator_key_id: "k1".into(),
            account_key_id: "k2".into(),
            tls_key_id: "k3".into(),
            system_account: "ASYS".into(),
            hub_account: "AHUB".into(),
            cert_der: vec![0x30, 0x00, 0xff],
            spki_sha256: "ab".repeat(32),
            issued: BTreeMap::new(),
            removed: BTreeMap::new(),
            revocations: BTreeMap::from([("UOLD".to_string(), 7)]),
            revocations_pushed: false,
        };
        record.issued.insert(
            "f".repeat(32),
            Issued {
                box_id: "0".repeat(32),
                leaf_nkey: "UKEY".into(),
                issued_at: 5,
                previous_keys: BTreeMap::from([("UOLDER".to_string(), 3)]),
            },
        );
        assert!(matches!(read(dir.path()), RecordFile::Absent));
        write(dir.path(), &record).unwrap();
        match read(dir.path()) {
            RecordFile::Present(read) => assert_eq!(*read, record),
            _ => panic!("the record did not read back"),
        }
        std::fs::write(record_path(dir.path()), b"{").unwrap();
        assert!(matches!(read(dir.path()), RecordFile::Damaged { .. }));
    }
}
