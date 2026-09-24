//! The census value: one per live module process, under the key
//! `AccountNames::census_key(module_id)` in the account's census bucket.
//!
//! Revocation reads `credential_public` and `user_jwt_id` out of it. A respawn
//! overwrites the key at a higher `spawn_generation` with `credential_epoch` 0, and a
//! re-issue for the same generation overwrites it at the next epoch.

use serde_json::{json, Value};

/// The version of this value's layout, carried in `schema_versions.census` so a reader
/// can refuse a layout it does not know.
pub const CENSUS_SCHEMA_VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CensusValue {
    pub credential_public: String,
    pub user_jwt_id: String,
    pub spawn_generation: u64,
    pub credential_epoch: u64,
    /// The agent ids the credential is bound to.
    pub identities: Vec<String>,
    /// The room ids the credential is bound to.
    pub rooms: Vec<String>,
}

impl CensusValue {
    pub fn to_json(&self) -> Value {
        json!({
            "credential_public": self.credential_public,
            "user_jwt_id": self.user_jwt_id,
            "spawn_generation": self.spawn_generation,
            "credential_epoch": self.credential_epoch,
            "schema_versions": { "census": CENSUS_SCHEMA_VERSION },
            "identities": self.identities,
            "rooms": self.rooms,
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.to_json()).expect("a census value always encodes")
    }

    /// Reads a stored value back, refusing one that lacks a field or carries an unknown
    /// layout version.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let value: Value =
            serde_json::from_slice(bytes).map_err(|error| format!("not JSON: {error}"))?;
        let text = |name: &str| {
            value[name]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{name} is missing or not a string"))
        };
        let number = |name: &str| {
            value[name]
                .as_u64()
                .ok_or_else(|| format!("{name} is missing or not an unsigned integer"))
        };
        let list = |name: &str| -> Result<Vec<String>, String> {
            value[name]
                .as_array()
                .ok_or_else(|| format!("{name} is not a list"))?
                .iter()
                .map(|entry| {
                    entry
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| format!("{name} holds a non-string"))
                })
                .collect()
        };
        match value["schema_versions"]["census"].as_u64() {
            Some(CENSUS_SCHEMA_VERSION) => {}
            other => return Err(format!("unknown census layout version {other:?}")),
        }
        Ok(Self {
            credential_public: text("credential_public")?,
            user_jwt_id: text("user_jwt_id")?,
            spawn_generation: number("spawn_generation")?,
            credential_epoch: number("credential_epoch")?,
            identities: list("identities")?,
            rooms: list("rooms")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_value_carries_every_field_the_census_names_and_round_trips() {
        let value = CensusValue {
            credential_public: "UABC".to_string(),
            user_jwt_id: "JTI".to_string(),
            spawn_generation: 3,
            credential_epoch: 1,
            identities: vec![],
            rooms: vec![],
        };
        let json = value.to_json();
        for field in [
            "credential_public",
            "user_jwt_id",
            "spawn_generation",
            "credential_epoch",
            "schema_versions",
            "identities",
            "rooms",
        ] {
            assert!(json.get(field).is_some(), "census value lacks {field}");
        }
        assert_eq!(CensusValue::parse(&value.to_bytes()).unwrap(), value);
    }
}
