//! Revocation: ending a per-process credential ck-bus issued, as NATS state and never as
//! a vault delete.
//!
//! Three ordered steps, each idempotent:
//! 1. Add the user's public key to the box account JWT's `revocations`. The account JWT
//!    is read from the server through the claims lookup immediately before the update,
//!    never from a cache (ck-bus is its single writer). The operator signer (never the
//!    operator root) signs the updated JWT through `credential.sign`, and it is pushed
//!    over the system user's claims-update subject. The push counts only once the
//!    lookup reads back exactly the pushed token: the server saves a claims update
//!    without checking its issuer, so the update's own reply proves nothing. From then
//!    on the server refuses the user's JWT on every connect, across a server restart,
//!    and closes its live connections itself.
//! 2. Delete the module's census key, but only while it still names this credential
//!    (the same generation, epoch and key), as a compare-and-delete on its revision. A
//!    superseded credential's key was already overwritten by its successor, which is
//!    left alone.
//! 3. Kick each live connection of the user over `$SYS`. Connections are known from the
//!    server's connect events (`connections`); one that is already gone is not an
//!    error.
//!
//! Progress is durable (`progress`): a record with step 0 and the inputs is fsynced
//! before step (1), replaced after each step, and removed after step (3). On restart
//! every record resumes at the step after its last completed one. A damaged record is
//! recovered from the census entry for its module: at exactly its (generation, epoch),
//! the inputs are re-derived and the steps replay; absent or at another pair, step (2)
//! had committed, so the record is cleared and nothing is issued; unreadable, recovery
//! waits for the next period.
//!
//! What triggers a revocation here: a module fetching a new credential while its census
//! entry names another one. The entry read just before the issue is the superseded
//! credential, whether this process issued it or an earlier ck-bus process did (which is
//! how a restart finds superseded users: the census outlives the process, the in-memory
//! record of issued credentials does not). The other trigger, a census entry whose
//! module has no live process in the supervisor's spawn snapshot (the process exited),
//! belongs to ck-bus's spawn-stream consumer, which calls `Revoker::revoke_module`.
//!
//! JWT expiry is the other half of revocation (R16): every user JWT expires 15 minutes
//! after issue, and a key whose revocation is recorded is never renewed, so a
//! credential whose revocation was lost to damage stays valid for at most 15 minutes.

pub mod connections;
pub mod handler;
pub mod progress;

use std::{
    collections::BTreeSet,
    fmt,
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use cortexkit_bus_naming::AccountNames;
use serde_json::{json, Value};

use crate::{
    bootstrap::{
        account_jwt::{decode_claims, revocations, sign_account_jwt, AccountClaims},
        plane::{apply_account_jwt, ApplyError, BoxPlane, SystemPlane},
        Ready,
    },
    credentials::{roots::RootCredential, Credentials},
    grants,
    issuance::census::CensusValue,
};
use connections::Connections;
use progress::{Entry, Identity, KickTarget, ProgressStore, Record, LAST_STEP};

/// Causes named in the revocation log lines and in a deferral.
pub mod cause {
    pub const CENSUS_READ_FAILED: &str = "census-read-failed";
    pub const CENSUS_VALUE_DAMAGED: &str = "census-value-damaged";
    pub const CENSUS_DELETE_FAILED: &str = "census-delete-failed";
    pub const CLAIMS_LOOKUP_FAILED: &str = "claims-lookup-failed";
    pub const CLAIMS_UPDATE_FAILED: &str = "claims-update-failed";
    pub const CLAIMS_READBACK_MISMATCH: &str = "claims-readback-mismatch";
    pub const OPERATOR_SIGNATURE_REFUSED: &str = "operator-signature-refused";
    pub const ACCOUNT_JWT_UNUSABLE: &str = "account-jwt-unusable";
    /// The looked-up account JWT carries a claim ck-bus does not write, so another writer
    /// exists and a rebuilt update would drop that claim.
    pub const ACCOUNT_JWT_FOREIGN_CLAIM: &str = "account-jwt-foreign-claim";
    pub const KICK_FAILED: &str = "kick-failed";
    pub const PROGRESS_UNWRITABLE: &str = "progress-unwritable";
}

/// ck-bus's two connections and the box account they serve.
#[derive(Clone)]
pub struct RevocationPlane {
    pub names: AccountNames,
    pub account_public: String,
    pub system: Arc<dyn SystemPlane>,
    pub box_plane: Arc<dyn BoxPlane>,
}

impl RevocationPlane {
    pub fn from_ready(ready: &Ready) -> Option<Self> {
        Some(Self {
            names: grants::derive_account(&ready.account.acct).ok()?,
            account_public: ready.account.account_public.clone(),
            system: ready.system.clone(),
            box_plane: ready.box_plane.clone(),
        })
    }
}

/// The credential to revoke, as the census value named it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub identity: Identity,
    pub user_public: String,
    pub user_jwt_id: String,
}

impl Target {
    pub fn from_census(module_id: &str, value: &CensusValue) -> Self {
        Self {
            identity: Identity {
                module_id: module_id.to_string(),
                spawn_generation: value.spawn_generation,
                credential_epoch: value.credential_epoch,
            },
            user_public: value.credential_public.clone(),
            user_jwt_id: value.user_jwt_id.clone(),
        }
    }
}

/// What a completed revocation did in this run. Steps that an earlier run had already
/// completed, or that found nothing to do, report `false` or 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Completed {
    pub pushed: bool,
    pub census_deleted: bool,
    pub kicked: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevocationError {
    /// The census could not be read, so nothing is known to revoke; nothing was written.
    CensusUnreadable(String),
    /// A step could not complete; its record stays at `completed_step` and the step is
    /// retried on the next period.
    Deferred {
        completed_step: u8,
        cause: &'static str,
        message: String,
    },
    /// A test stopped the act at a boundary, exactly as a process death there would.
    #[cfg(test)]
    Stopped(Boundary),
}

impl fmt::Display for RevocationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CensusUnreadable(message) => write!(f, "census unreadable: {message}"),
            Self::Deferred {
                completed_step,
                cause,
                message,
            } => write!(
                f,
                "deferred after step {completed_step} ({cause}): {message}"
            ),
            #[cfg(test)]
            Self::Stopped(boundary) => write!(f, "stopped at {boundary:?}"),
        }
    }
}

fn deferred(
    completed_step: u8,
    cause: &'static str,
    message: impl Into<String>,
) -> RevocationError {
    RevocationError::Deferred {
        completed_step,
        cause,
        message: message.into(),
    }
}

/// The places a test can stop a revocation: just after a record is written, and just
/// after a step's effect and before its record.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boundary {
    ZeroRecorded,
    Step1Done,
    Step1Recorded,
    Step2Done,
    Step2Recorded,
    Step3Done,
}

pub struct Revoker {
    credentials: Arc<Credentials>,
    progress: ProgressStore,
    connections: Arc<Connections>,
    /// One revocation at a time: each step (1) is a read-modify-write of the one account
    /// JWT, and two at once would lose one of the two revocations.
    serial: tokio::sync::Mutex<()>,
    #[cfg(test)]
    pub stop_at: std::sync::Mutex<Option<Boundary>>,
}

impl Revoker {
    pub fn new(
        credentials: Arc<Credentials>,
        store_root: &Path,
        connections: Arc<Connections>,
    ) -> Self {
        let progress = ProgressStore::new(store_root);
        progress.remove_stale_tmp();
        Self {
            credentials,
            progress,
            connections,
            serial: tokio::sync::Mutex::new(()),
            #[cfg(test)]
            stop_at: std::sync::Mutex::new(None),
        }
    }

    pub fn progress(&self) -> &ProgressStore {
        &self.progress
    }

    pub fn connections(&self) -> &Arc<Connections> {
        &self.connections
    }

    #[cfg(test)]
    fn stop(&self, boundary: Boundary) -> Result<(), RevocationError> {
        let stop = *self
            .stop_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if stop == Some(boundary) {
            return Err(RevocationError::Stopped(boundary));
        }
        Ok(())
    }

    /// Revokes the credential the module's census entry names now. The census read is
    /// step (1)'s input: a read that fails revokes nothing and writes nothing.
    /// `Ok(None)` when the module has no entry.
    pub async fn revoke_module(
        &self,
        plane: &RevocationPlane,
        module_id: &str,
    ) -> Result<Option<Completed>, RevocationError> {
        let key = AccountNames::census_key(module_id)
            .map_err(|error| RevocationError::CensusUnreadable(error.to_string()))?;
        let record = plane
            .box_plane
            .census_get(&plane.names, &key)
            .await
            .map_err(|error| RevocationError::CensusUnreadable(error.message))?;
        let Some(record) = record else {
            return Ok(None);
        };
        let value = CensusValue::parse(&record.value).map_err(|reason| {
            RevocationError::CensusUnreadable(format!(
                "the census value for {key} is damaged: {reason}"
            ))
        })?;
        self.revoke(plane, Target::from_census(module_id, &value))
            .await
            .map(Some)
    }

    /// Records a revocation (step 0, with its inputs and the kick targets known now) so
    /// that it survives a restart. A record already in progress for the same identity
    /// is kept, never set back.
    pub fn begin(&self, target: &Target) -> Result<(), RevocationError> {
        // Before anything is pushed, so no renewal of this key can outlive its
        // revocation (see `KeyCustody::mark_revoked`).
        self.credentials.custody.mark_revoked(&target.user_public);
        if let Some(Entry::Present(_)) = self.progress.read(&target.identity) {
            return Ok(());
        }
        let record = Record {
            identity: target.identity.clone(),
            highest_completed_step: 0,
            user_public: target.user_public.clone(),
            user_jwt_id: target.user_jwt_id.clone(),
            kick: self.connections.targets(&target.user_public),
        };
        self.write(&record)?;
        log_event("ckbus.revocation.recorded", record_fields(&record));
        Ok(())
    }

    /// Records and runs one revocation to completion.
    pub async fn revoke(
        &self,
        plane: &RevocationPlane,
        target: Target,
    ) -> Result<Completed, RevocationError> {
        self.begin(&target)?;
        #[cfg(test)]
        self.stop(Boundary::ZeroRecorded)?;
        match self.progress.read(&target.identity) {
            Some(Entry::Present(record)) => self.drive(plane, record).await,
            _ => Err(deferred(
                0,
                cause::PROGRESS_UNWRITABLE,
                format!(
                    "{} did not read back after it was written",
                    self.progress.path(&target.identity).display()
                ),
            )),
        }
    }

    /// Resumes every recorded revocation, damaged ones through the census. Returns each
    /// identity with its outcome; a deferred one keeps its record for the next call.
    pub async fn resume_all(
        &self,
        plane: &RevocationPlane,
    ) -> Vec<(Identity, Result<Completed, RevocationError>)> {
        let entries = match self.progress.list() {
            Ok(entries) => entries,
            Err(error) => {
                log_event(
                    "ckbus.revocation.progress_unlisted",
                    json!({
                        "path": self.progress.dir().display().to_string(),
                        "error": error.to_string(),
                    }),
                );
                return Vec::new();
            }
        };
        let mut outcomes = Vec::new();
        for entry in entries {
            let (identity, outcome) = match entry {
                Entry::Present(record) => {
                    let identity = record.identity.clone();
                    (identity, self.drive(plane, record).await)
                }
                Entry::Damaged {
                    identity,
                    path,
                    reason,
                } => {
                    let outcome = self.recover(plane, &identity, &path, &reason).await;
                    (identity, outcome)
                }
            };
            if let Err(error) = &outcome {
                log_event(
                    "ckbus.revocation.deferred",
                    json!({
                        "module_id": identity.module_id,
                        "spawn_generation": identity.spawn_generation,
                        "credential_epoch": identity.credential_epoch,
                        "reason": error.to_string(),
                    }),
                );
            }
            outcomes.push((identity, outcome));
        }
        outcomes
    }

    /// A damaged record: its key, jwt id and kick targets are lost, so the census entry
    /// for its module decides (see the module documentation).
    async fn recover(
        &self,
        plane: &RevocationPlane,
        identity: &Identity,
        path: &Path,
        reason: &str,
    ) -> Result<Completed, RevocationError> {
        let key = AccountNames::census_key(&identity.module_id)
            .map_err(|error| deferred(0, cause::CENSUS_READ_FAILED, error.to_string()))?;
        // Absence concludes something only from a read that succeeded.
        let entry = plane
            .box_plane
            .census_get(&plane.names, &key)
            .await
            .map_err(|error| deferred(0, cause::CENSUS_READ_FAILED, error.message))?;
        let current = entry
            .as_ref()
            .and_then(|entry| CensusValue::parse(&entry.value).ok())
            .filter(|value| {
                value.spawn_generation == identity.spawn_generation
                    && value.credential_epoch == identity.credential_epoch
            });
        let Some(value) = current else {
            self.progress
                .clear(identity)
                .map_err(|error| deferred(0, cause::PROGRESS_UNWRITABLE, error.to_string()))?;
            log_event(
                "ckbus.revocation.recovered",
                json!({
                    "module_id": identity.module_id,
                    "spawn_generation": identity.spawn_generation,
                    "credential_epoch": identity.credential_epoch,
                    "path": path.display().to_string(),
                    "damage": reason,
                    "case": if entry.is_none() { "census-entry-absent" } else { "census-entry-at-another-pair" },
                    "action": "cleared; step (2) had committed, so step (1) had too; nothing issued",
                }),
            );
            return Ok(Completed::default());
        };
        let record = Record {
            identity: identity.clone(),
            highest_completed_step: 0,
            user_public: value.credential_public.clone(),
            user_jwt_id: value.user_jwt_id.clone(),
            kick: self.connections.targets(&value.credential_public),
        };
        self.write(&record)?;
        log_event(
            "ckbus.revocation.recovered",
            json!({
                "module_id": identity.module_id,
                "spawn_generation": identity.spawn_generation,
                "credential_epoch": identity.credential_epoch,
                "path": path.display().to_string(),
                "damage": reason,
                "case": "census-entry-at-this-pair",
                "action": "inputs re-derived from the census entry; replaying from step (1)",
            }),
        );
        self.drive(plane, record).await
    }

    /// Runs the steps after `record.highest_completed_step`, recording each.
    async fn drive(
        &self,
        plane: &RevocationPlane,
        mut record: Record,
    ) -> Result<Completed, RevocationError> {
        let _serial = self.serial.lock().await;
        let mut completed = Completed::default();
        if record.highest_completed_step < 1 {
            completed.pushed = self.push_revocation(plane, &record).await?;
            #[cfg(test)]
            self.stop(Boundary::Step1Done)?;
            record.highest_completed_step = 1;
            self.write(&record)?;
            #[cfg(test)]
            self.stop(Boundary::Step1Recorded)?;
        }
        if record.highest_completed_step < 2 {
            completed.census_deleted = self.delete_census(plane, &record).await?;
            #[cfg(test)]
            self.stop(Boundary::Step2Done)?;
            record.highest_completed_step = 2;
            self.write(&record)?;
            #[cfg(test)]
            self.stop(Boundary::Step2Recorded)?;
        }
        if record.highest_completed_step < LAST_STEP {
            completed.kicked = self.kick(plane, &record).await?;
        }
        #[cfg(test)]
        self.stop(Boundary::Step3Done)?;
        self.progress
            .clear(&record.identity)
            .map_err(|error| deferred(LAST_STEP, cause::PROGRESS_UNWRITABLE, error.to_string()))?;
        let mut fields = record_fields(&record);
        if let Value::Object(fields) = &mut fields {
            fields.insert("pushed".to_string(), json!(completed.pushed));
            fields.insert(
                "census_deleted".to_string(),
                json!(completed.census_deleted),
            );
            fields.insert("kicked".to_string(), json!(completed.kicked));
        }
        log_event("ckbus.revocation.completed", fields);
        Ok(completed)
    }

    fn write(&self, record: &Record) -> Result<(), RevocationError> {
        self.progress.write(record).map_err(|error| {
            deferred(
                record.highest_completed_step.saturating_sub(1),
                cause::PROGRESS_UNWRITABLE,
                format!(
                    "{}: {error}",
                    self.progress.path(&record.identity).display()
                ),
            )
        })
    }

    /// Step (1). `Ok(true)` when this call pushed the update, `Ok(false)` when the
    /// looked-up account JWT already carried the revocation (that lookup is itself the
    /// read-back, and a second push would change nothing).
    async fn push_revocation(
        &self,
        plane: &RevocationPlane,
        record: &Record,
    ) -> Result<bool, RevocationError> {
        let current = plane
            .system
            .lookup(&plane.account_public)
            .await
            .map_err(|error| deferred(0, cause::CLAIMS_LOOKUP_FAILED, error.message))?
            .ok_or_else(|| {
                deferred(
                    0,
                    cause::CLAIMS_LOOKUP_FAILED,
                    format!("the resolver holds no JWT for {}", plane.account_public),
                )
            })?;
        let claims = decode_claims(&current).ok_or_else(|| {
            deferred(
                0,
                cause::ACCOUNT_JWT_UNUSABLE,
                "the looked-up account JWT does not decode",
            )
        })?;
        if claims["sub"].as_str() != Some(plane.account_public.as_str()) {
            return Err(deferred(
                0,
                cause::ACCOUNT_JWT_UNUSABLE,
                format!(
                    "the lookup for {} answered a JWT for {:?}",
                    plane.account_public, claims["sub"]
                ),
            ));
        }
        let mut revoked = revocations(&claims);
        if revoked.contains_key(&record.user_public) {
            return Ok(false);
        }
        let signing_keys = claims["nats"]["signing_keys"]
            .as_array()
            .map(|keys| {
                keys.iter()
                    .map(|key| key.as_str().map(str::to_string))
                    .collect::<Option<Vec<_>>>()
            })
            .unwrap_or(Some(Vec::new()))
            .ok_or_else(|| {
                deferred(
                    0,
                    cause::ACCOUNT_JWT_UNUSABLE,
                    "the account JWT lists a signing key that is not a plain key; ck-bus writes \
                     only plain keys",
                )
            })?;
        let name = claims["name"].as_str().unwrap_or_default().to_string();
        let previous_iat = claims["iat"].as_i64().unwrap_or_default();
        // The update below is rebuilt from ck-bus's own claim layout (`AccountClaims`), not
        // edited in place, so it drops any claim ck-bus does not write itself. That is safe
        // only while ck-bus is the account's single writer. So the looked-up claims must be
        // exactly what ck-bus would have written for them; a claim it did not write means
        // another writer, and the push is refused naming that claim rather than dropping
        // it silently.
        let as_written = AccountClaims {
            account_public: plane.account_public.clone(),
            name: name.clone(),
            signing_keys: signing_keys.clone(),
            revocations: revoked.clone(),
            issued_at: previous_iat,
        }
        .claims(claims["iss"].as_str().unwrap_or_default());
        if let Some(claim) = foreign_claim(&claims, &as_written, "") {
            return Err(deferred(
                0,
                cause::ACCOUNT_JWT_FOREIGN_CLAIM,
                format!(
                    "the box account JWT carries `{claim}`, which ck-bus does not write; \
                     rebuilding it for the revocation would drop it, so nothing is pushed"
                ),
            ));
        }
        let now = unix_now();
        revoked.insert(record.user_public.clone(), now);
        // The server keeps the newer of two account JWTs by `iat`, so an update in the
        // same second as the one it replaces is dated one second later.
        let updated = AccountClaims {
            account_public: plane.account_public.clone(),
            name,
            signing_keys,
            revocations: revoked,
            issued_at: now.max(previous_iat + 1),
        };
        let signer_id = RootCredential::OperatorSigner
            .credential_id()
            .map_err(|error| deferred(0, cause::OPERATOR_SIGNATURE_REFUSED, error.to_string()))?;
        let jwt = sign_account_jwt(
            self.credentials.vault.as_ref(),
            &self.credentials.key_ids,
            &signer_id,
            &updated,
        )
        .await
        .map_err(|error| deferred(0, cause::OPERATOR_SIGNATURE_REFUSED, error.to_string()))?;
        apply_account_jwt(plane.system.as_ref(), &plane.account_public, &jwt)
            .await
            .map_err(|error| match error {
                ApplyError::ReadBackMismatch { .. } => {
                    deferred(0, cause::CLAIMS_READBACK_MISMATCH, error.to_string())
                }
                ApplyError::Plane(error) => deferred(0, cause::CLAIMS_UPDATE_FAILED, error.message),
            })?;
        Ok(true)
    }

    /// Step (2). `Ok(true)` when this call deleted the key.
    async fn delete_census(
        &self,
        plane: &RevocationPlane,
        record: &Record,
    ) -> Result<bool, RevocationError> {
        let key = AccountNames::census_key(&record.identity.module_id)
            .map_err(|error| deferred(1, cause::CENSUS_READ_FAILED, error.to_string()))?;
        let names_this = |entry: &Option<progress_census::Stored>| {
            entry.as_ref().is_some_and(|stored| stored.names(record))
        };
        let entry = progress_census::read(plane, &key).await?;
        if !names_this(&entry) {
            return Ok(false);
        }
        let revision = entry.map(|stored| stored.revision).unwrap_or_default();
        if let Err(error) = plane
            .box_plane
            .census_delete(&plane.names, &key, revision)
            .await
        {
            // A write between the read and the delete moved the revision; the key then
            // names another credential and is not this revocation's to delete.
            if names_this(&progress_census::read(plane, &key).await?) {
                return Err(deferred(1, cause::CENSUS_DELETE_FAILED, error.message));
            }
            return Ok(false);
        }
        Ok(true)
    }

    /// Step (3). Returns how many connections were kicked.
    async fn kick(
        &self,
        plane: &RevocationPlane,
        record: &Record,
    ) -> Result<usize, RevocationError> {
        let mut targets: BTreeSet<KickTarget> = record.kick.clone();
        targets.extend(self.connections.targets(&record.user_public));
        let mut kicked = 0;
        for target in targets {
            match plane.system.kick(&target.server_id, target.client_id).await {
                Ok(()) => kicked += 1,
                // The push in step (1) closes the user's connections itself (measured
                // against nats-server 2.15.0: the disconnect event's reason is
                // "Credentials Revoked"), so the kick is a backstop that normally finds
                // nothing left.
                Err(error) if kick_target_gone(&error.message) => {}
                Err(error) => return Err(deferred(2, cause::KICK_FAILED, error.message)),
            }
        }
        Ok(kicked)
    }
}

/// The first claim, as a dotted path, where `found` differs from `written` (what ck-bus
/// would have written for the same account): a member only one side has, or a differing
/// value. The top-level `jti` is skipped: ck-bus derives it from the other claims, and a
/// JWT signed by anyone else carries its own. `None` when they agree.
fn foreign_claim(found: &Value, written: &Value, path: &str) -> Option<String> {
    let join = |key: &str| {
        if path.is_empty() {
            key.to_string()
        } else {
            format!("{path}.{key}")
        }
    };
    match (found, written) {
        (Value::Object(found), Value::Object(written)) => {
            let skip = |key: &str| path.is_empty() && key == "jti";
            for (key, value) in found.iter().filter(|(key, _)| !skip(key)) {
                match written.get(key) {
                    None => return Some(join(key)),
                    Some(ours) => {
                        if let Some(claim) = foreign_claim(value, ours, &join(key)) {
                            return Some(claim);
                        }
                    }
                }
            }
            written
                .keys()
                .find(|key| !skip(key) && !found.contains_key(*key))
                .map(|key| join(key))
        }
        _ if found == written => None,
        _ => Some(if path.is_empty() {
            "(the claims object)".to_string()
        } else {
            path.to_string()
        }),
    }
}

/// Whether a refused kick means the connection no longer exists. nats-server 2.15.0
/// answers a kick for a client id it does not hold with
/// `{"code":500,"description":"no such client or leafnode id"}`; the code is a generic
/// 500, so the exact description is what identifies the case. Any other refusal fails
/// the step.
fn kick_target_gone(message: &str) -> bool {
    message.contains(r#""description":"no such client or leafnode id""#)
}

/// Step (2)'s census read, named so the "does the entry still name this credential"
/// rule is stated once.
mod progress_census {
    use super::{cause, deferred, Record, RevocationError, RevocationPlane};
    use crate::issuance::census::CensusValue;

    pub struct Stored {
        pub value: Option<CensusValue>,
        pub revision: u64,
    }

    impl Stored {
        /// The entry names exactly this revocation's credential. A value that does not
        /// parse names nothing, so it is never deleted here.
        pub fn names(&self, record: &Record) -> bool {
            self.value.as_ref().is_some_and(|value| {
                value.spawn_generation == record.identity.spawn_generation
                    && value.credential_epoch == record.identity.credential_epoch
                    && value.credential_public == record.user_public
            })
        }
    }

    pub async fn read(
        plane: &RevocationPlane,
        key: &str,
    ) -> Result<Option<Stored>, RevocationError> {
        let entry = plane
            .box_plane
            .census_get(&plane.names, key)
            .await
            .map_err(|error| deferred(1, cause::CENSUS_READ_FAILED, error.message))?;
        Ok(entry.map(|entry| Stored {
            value: CensusValue::parse(&entry.value).ok(),
            revision: entry.revision,
        }))
    }
}

fn record_fields(record: &Record) -> Value {
    json!({
        "module_id": record.identity.module_id,
        "spawn_generation": record.identity.spawn_generation,
        "credential_epoch": record.identity.credential_epoch,
        "user_public": record.user_public,
        "user_jwt_id": record.user_jwt_id,
        "kick_targets": record.kick.len(),
    })
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

/// One structured stderr line, in the same shape as bootstrap's.
pub(crate) fn log_event(event: &str, fields: Value) {
    let at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default();
    let mut line = json!({ "event": event, "at_ms": at_ms });
    if let (Some(line), Value::Object(fields)) = (line.as_object_mut(), fields) {
        line.extend(fields);
    }
    eprintln!("{line}");
}

#[cfg(test)]
mod tests {
    use super::{foreign_claim, kick_target_gone};
    use serde_json::json;

    #[test]
    fn a_claim_ck_bus_does_not_write_is_named_by_its_path() {
        let written = json!({"jti": "A", "iat": 1, "nats": {"limits": {"conn": -1}}});
        assert_eq!(foreign_claim(&written, &written, ""), None);
        let mut found = written.clone();
        found["jti"] = json!("B");
        assert_eq!(foreign_claim(&found, &written, ""), None, "jti is derived");
        found["nats"]["imports"] = json!([]);
        assert_eq!(
            foreign_claim(&found, &written, "").as_deref(),
            Some("nats.imports")
        );
        let mut changed = written.clone();
        changed["nats"]["limits"]["conn"] = json!(5);
        assert_eq!(
            foreign_claim(&changed, &written, "").as_deref(),
            Some("nats.limits.conn")
        );
        let mut missing = written.clone();
        missing["nats"]["limits"] = json!({});
        assert_eq!(
            foreign_claim(&missing, &written, "").as_deref(),
            Some("nats.limits.conn")
        );
    }

    #[test]
    fn only_the_servers_exact_no_such_client_answer_reads_as_gone() {
        assert!(kick_target_gone(
            r#"kick refused: {"code":500,"description":"no such client or leafnode id"}"#
        ));
        for other in [
            r#"kick refused: {"code":500,"description":"internal error"}"#,
            r#"kick refused: {"code":404,"description":"not found"}"#,
            "$SYS.REQ.SERVER.NBOGUS.KICK: no responders",
            "$SYS.REQ.SERVER.N.KICK: no reply within 5s",
        ] {
            assert!(!kick_target_gone(other), "{other}");
        }
    }
}
