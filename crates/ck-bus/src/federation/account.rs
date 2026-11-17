//! The federation account: a second account on the local server, `fed_<machine id>`,
//! which the leaf remote binds instead of the box account.
//!
//! A leaf forwards any message on its bound account that the far side shows interest
//! in, so binding the box account would let a hub-side subscription pull this machine's
//! local, unsealed traffic across. The federation account holds only the sealed
//! cross-machine families, and ck-bus is the only thing that moves messages between it
//! and the box account.
//!
//! It is found or created exactly the way bootstrap finds the box account: by the id
//! `fed_account.json` records, or, when that file is absent, by listing the resolver's
//! accounts and adopting the one with its name. Only when none exists is an account
//! identity key generated, once, in memory; its public half is recorded before anything
//! is pushed and the seed is dropped. Each process then re-signs the account JWT with
//! the operator signer, pushes it through the system account and reads it back.
//!
//! JetStream is enabled (every limit explicit, as for the box account), but no stream is
//! created here: the message path creates its outbox and inbox when it lands.

use nkeys::KeyPair;
use serde_json::Value;

use crate::{
    bootstrap::{
        account_jwt::AccountClaims,
        account_jwt::{decode_claims, prune_revocations, revocations, sign_account_jwt},
        plane::{apply_account_jwt, SystemPlane},
        store::{AccountFile, AccountRecord, Store},
    },
    credentials::{roots::RootCredential, Credentials},
};

/// The federation account's name for a machine id.
pub fn federation_account_name(machine_id: &str) -> String {
    format!("fed_{machine_id}")
}

/// Finds or creates the federation account, signs and pushes its JWT, and returns its
/// public key. Every refusal names its cause; nothing is retried here.
pub async fn ensure_federation_account(
    system: &dyn SystemPlane,
    credentials: &Credentials,
    store: &Store,
    machine_id: &str,
) -> Result<String, String> {
    let acct = federation_account_name(machine_id);
    let (record, existing) = match store.read_federation_account() {
        AccountFile::Damaged { path, reason } => {
            return Err(format!(
                "{} is damaged ({reason}); it is left as it is",
                path.display()
            ))
        }
        AccountFile::Present(record) if record.machine_id != machine_id => {
            return Err(format!(
                "{} records machine id {}, this machine is {machine_id}; ck-bus creates no \
                 second federation account",
                store.federation_account_path().display(),
                record.machine_id
            ))
        }
        AccountFile::Present(record) => {
            let existing = system
                .lookup(&record.account_public)
                .await
                .map_err(|error| error.message)?
                .and_then(|jwt| decode_claims(&jwt));
            (record, existing)
        }
        AccountFile::Absent => adopt_or_create(system, store, machine_id, &acct).await?,
    };
    if let Some(name) = existing.as_ref().and_then(|claims| claims["name"].as_str()) {
        if name != record.acct {
            return Err(format!(
                "{} names {} for {}, the server's JWT names {name:?}",
                store.federation_account_path().display(),
                record.acct,
                record.account_public
            ));
        }
    }

    let signer_id = RootCredential::OperatorSigner
        .credential_id()
        .map_err(|refusal| refusal.to_string())?;
    let now = unix_now();
    let mut revoked = existing.as_ref().map(revocations).unwrap_or_default();
    prune_revocations(&mut revoked, now, credentials.lifetime);
    // The server keeps the newer of two account JWTs by `iat`.
    let previous_iat = existing
        .as_ref()
        .and_then(|claims| claims["iat"].as_i64())
        .unwrap_or_default();
    let claims = AccountClaims {
        account_public: record.account_public.clone(),
        name: record.acct.clone(),
        // No signing key yet, unlike the box account: the leaf remote binds this account
        // without any user in it, so nothing needs to sign one. The message path adds the
        // federation root (`RootCredential::FederationAccount`) here, and re-issues this
        // JWT, when it first needs a federation-account user.
        signing_keys: Vec::new(),
        revocations: revoked,
        issued_at: now.max(previous_iat + 1),
    };
    let jwt = sign_account_jwt(
        credentials.vault.as_ref(),
        &credentials.key_ids,
        &signer_id,
        &claims,
    )
    .await
    .map_err(|error| error.to_string())?;
    apply_account_jwt(system, &record.account_public, &jwt)
        .await
        .map_err(|error| error.to_string())?;
    Ok(record.account_public)
}

/// With no record, adopts the one account on the server named `acct`, or creates the
/// account identity. The record is written before any push, so a crash after the push
/// still finds the id.
async fn adopt_or_create(
    system: &dyn SystemPlane,
    store: &Store,
    machine_id: &str,
    acct: &str,
) -> Result<(AccountRecord, Option<Value>), String> {
    let mut matching = Vec::new();
    for id in system
        .list_accounts()
        .await
        .map_err(|error| error.message)?
    {
        let Some(claims) = system
            .lookup(&id)
            .await
            .map_err(|error| error.message)?
            .and_then(|jwt| decode_claims(&jwt))
        else {
            continue;
        };
        if claims["name"].as_str() == Some(acct) {
            matching.push((id, claims));
        }
    }
    if matching.len() > 1 {
        return Err(format!(
            "the server holds {} accounts named {acct}: {:?}",
            matching.len(),
            matching.iter().map(|(id, _)| id).collect::<Vec<_>>()
        ));
    }
    let (account_public, existing) = match matching.pop() {
        Some((id, claims)) => (id, Some(claims)),
        // Only the public half is kept; nobody needs the seed, because any user of this
        // account will be signed by a signing key the account JWT lists.
        None => (KeyPair::new_account().public_key(), None),
    };
    let record = AccountRecord {
        machine_id: machine_id.to_string(),
        acct: acct.to_string(),
        account_public,
    };
    store
        .write_federation_account(&record)
        .map_err(|error| format!("{}: {error}", store.federation_account_path().display()))?;
    Ok((record, existing))
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
