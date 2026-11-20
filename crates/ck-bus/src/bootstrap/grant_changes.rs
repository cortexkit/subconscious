//! Retire stale permissions before exposing bootstrap's box plane to issuance.

use super::{
    account_jwt::{sign_account_jwt, AccountClaims},
    cause,
    census::{recover_credential_public, CensusValue},
    log_event,
    plane::{apply_account_jwt, ApplyError, BoxPlane, SystemPlane},
    unix_now, Failure,
};
use crate::{credentials::Credentials, grants};
use cortexkit_bus_naming::AccountNames;
use serde_json::json;

pub(crate) async fn revoke_changed(
    credentials: &Credentials,
    system: &dyn SystemPlane,
    box_plane: &dyn BoxPlane,
    names: &AccountNames,
    own_module: &str,
    signer_id: &str,
    claims: &mut AccountClaims,
) -> Result<(), Failure> {
    let census_failure = |message| Failure::retry(cause::CENSUS_UNAVAILABLE, message);
    let mut changed = Vec::new();
    for module_id in box_plane
        .census_keys(names)
        .await
        .map_err(|e| census_failure(e.message))?
    {
        // ck-bus has its own grant and rotates its user on every boot already.
        if module_id == own_module || module_id.starts_with(super::census::HUB_PREFIX) {
            continue;
        }
        let Some(record) = box_plane
            .census_get(names, &module_id)
            .await
            .map_err(|e| census_failure(e.message))?
        else {
            continue;
        };
        let value = match CensusValue::parse(&record.value) {
            Ok(value) => value,
            Err(reason) => {
                log_event(
                    "ckbus.bootstrap.census_value_damaged",
                    json!({"level": "WARN", "key": module_id, "reason": reason}),
                );
                if let Some(key) = recover_credential_public(&record.value) {
                    if !claims.revocations.contains_key(&key) {
                        changed.push((module_id, key, "census_damaged"));
                    }
                }
                continue;
            }
        };
        if claims.revocations.contains_key(&value.credential_public) {
            continue;
        }
        // Reuse the existing key: its inbox is itself part of the permission set.
        // Issuance currently binds no rooms, regardless of older census metadata.
        let reason = match grants::issued_grant(names, &module_id, &value.credential_public, &[]) {
            Ok(grant)
                if value.grant_digest.as_deref() == Some(grant.permission_digest().as_str()) =>
            {
                continue
            }
            Ok(_) => "grant_changed",
            Err(_) => "grant_removed",
        };
        changed.push((module_id, value.credential_public, reason));
    }
    if changed.is_empty() {
        return Ok(());
    }
    let now = unix_now();
    for (_, user, _) in &changed {
        claims.revocations.insert(user.clone(), now);
    }
    claims.issued_at = now.max(claims.issued_at + 1);
    let jwt = sign_account_jwt(
        credentials.vault.as_ref(),
        &credentials.key_ids,
        signer_id,
        claims,
    )
    .await
    .map_err(|error| Failure::retry(cause::ROOT_KEY_UNREACHABLE, error.to_string()))?;
    // One account update retires every changed credential. Keep census rows until
    // replacement issuance; no consumer, stream, KV value, or flow state is deleted.
    apply_account_jwt(system, &claims.account_public, &jwt)
        .await
        .map_err(|error| match error {
            ApplyError::ReadBackMismatch { .. } => {
                Failure::retry(cause::CLAIMS_READBACK_MISMATCH, error.to_string())
            }
            ApplyError::Plane(error) => Failure::retry(cause::BROKER_UNAVAILABLE, error.message),
        })?;
    for (module_id, _, reason) in changed {
        log_event(
            "ckbus.bootstrap.module_revoked",
            json!({"module_id": module_id, "cause": reason}),
        );
    }
    Ok(())
}
