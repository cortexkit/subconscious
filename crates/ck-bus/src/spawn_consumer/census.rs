//! The census side of the spawn consumer: listing the census and revoking an entry,
//! through the revocation area's `Revoker` (the same one the superseded-credential path
//! uses, so every step (1) is serialized in one place and every revocation has one
//! durable progress record).

use std::sync::Arc;

use async_trait::async_trait;
use cortexkit_bus_naming::AccountNames;
use serde_json::json;
use tokio::sync::watch;

use super::{log_event, Census, CensusEntry, Fence, Revoked};
use crate::{
    bootstrap::Ready,
    issuance::census::CensusValue,
    revocation::{RevocationError, RevocationPlane, Revoker, Target},
};

pub struct RevokerCensus {
    revoker: Arc<Revoker>,
    ready: watch::Receiver<Option<Arc<Ready>>>,
}

impl RevokerCensus {
    pub fn new(revoker: Arc<Revoker>, ready: watch::Receiver<Option<Arc<Ready>>>) -> Self {
        Self { revoker, ready }
    }

    fn plane(&self) -> Result<RevocationPlane, String> {
        let ready = self
            .ready
            .borrow()
            .clone()
            .ok_or("bootstrap has not finished, so the census is not reachable yet")?;
        RevocationPlane::from_ready(&ready)
            .ok_or_else(|| "the box account's names do not derive".to_string())
    }
}

#[async_trait]
impl Census for RevokerCensus {
    async fn entries(&self) -> Result<Vec<CensusEntry>, String> {
        let plane = self.plane()?;
        let keys = plane
            .box_plane
            .census_keys(&plane.names)
            .await
            .map_err(|error| error.message)?;
        let mut entries = Vec::new();
        for key in keys {
            // The census key is the module id itself; a key that is not a module id was
            // not written by issuance and names no process.
            if AccountNames::census_key(&key).ok().as_deref() != Some(key.as_str()) {
                continue;
            }
            let Some(record) = plane
                .box_plane
                .census_get(&plane.names, &key)
                .await
                .map_err(|error| error.message)?
            else {
                continue;
            };
            match CensusValue::parse(&record.value) {
                Ok(value) => entries.push(CensusEntry {
                    module_id: key,
                    spawn_generation: value.spawn_generation,
                }),
                // Its generation is unknown, so whether its process lives is unknown too;
                // nothing is concluded from it.
                Err(reason) => log_event(
                    "ckbus.spawn.census_value_damaged",
                    json!({ "module_id": key, "reason": reason }),
                ),
            }
        }
        Ok(entries)
    }

    async fn revoke(&self, module_id: &str, fence: Fence) -> Result<Revoked, String> {
        let plane = self.plane()?;
        let key = AccountNames::census_key(module_id).map_err(|error| error.to_string())?;
        let Some(record) = plane
            .box_plane
            .census_get(&plane.names, &key)
            .await
            .map_err(|error| error.message)?
        else {
            return Ok(Revoked::NoEntry);
        };
        let value = CensusValue::parse(&record.value)
            .map_err(|reason| format!("the census value for {key} is damaged: {reason}"))?;
        if !fence.admits(value.spawn_generation) {
            return Ok(Revoked::Fenced {
                entry_generation: value.spawn_generation,
            });
        }
        match self
            .revoker
            .revoke(&plane, Target::from_census(module_id, &value))
            .await
        {
            Ok(_) => Ok(Revoked::Revoked {
                entry_generation: value.spawn_generation,
            }),
            // The record is durable; the revocation area retries it every period.
            Err(error @ RevocationError::Deferred { .. }) => Ok(Revoked::Deferred {
                entry_generation: value.spawn_generation,
                reason: error.to_string(),
            }),
            Err(error) => Err(error.to_string()),
        }
    }
}
