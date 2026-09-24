//! Issuance: a supervised participant asks ck-bus for its bus credential.
//!
//! ck-bus serves two ops over the subc wire, `ckbus.credential` and `ckbus.nonce_sign`.
//! Both authorize by the principal the daemon stamped on the caller's route at bind time
//! and by nothing the caller writes in its request: `Principal::Reserved { module_id }`
//! (the caller presented its launch nonce) binds the answer to that module id and to the
//! live spawn generation the supervisor's spawn snapshot shows for it. A body field
//! naming another module id is ignored. Any other principal is refused
//! `ckbus_principal_direct`, and a module with no live generation
//! `ckbus_generation_not_live`.
//!
//! Issuing and writing the census key are one act, in this order:
//! 1. fence against the spawn snapshot's generation;
//! 2. advance and fsync the generation's entry in `epoch_high_water.json`;
//! 3. generate the user key in memory and have the box account root sign its JWT;
//! 4. (retired by R15: agent durables are created by prefrontal through
//!    `ckbus.agent_durable_bind`, in the membership area, never at issuance);
//! 5. write the census key;
//! 6. answer.
//!
//! A failure (or a crash) before step 5 leaves a signed JWT whose seed is dropped and no
//! census entry, so there is nothing to roll back; the next request issues at a higher
//! epoch. Once an issue completes, the module's previous user is superseded: its key is
//! dropped from memory (so a reconnect under it gets `ckbus_credential_superseded`). Its
//! revocation is the revocation area's: it reads the census entry just before each issue,
//! so the superseded key is found there, whichever ck-bus process issued it.
//!
//! The grant names no agent (R15: agent access is account-scoped). `grants::issued_grant`
//! picks it from the attested module id: the delivery-authority grant for
//! `reserved:prefrontal-core`, the participant grant for every other module. Rooms have
//! no named source yet, so every module is issued with none.

pub mod census;
pub mod handler;
pub mod high_water;

use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use cortexkit_bus_naming::AccountNames;
use serde_json::{json, Value};

use crate::{
    bootstrap::plane::BoxPlane,
    credentials::{
        custody::CREDENTIAL_SUPERSEDED,
        issue::{sign_user_jwt, SignedUserJwt, UserJwtRequest},
        roots::RootCredential,
        Credentials,
    },
    grants::{self, Grant},
};
use census::CensusValue;
use high_water::{HighWater, HighWaterRefusal};

/// The op a participant calls to fetch its credential.
pub const CREDENTIAL_OP: &str = "ckbus.credential";
/// The op a participant calls to have its connect nonce signed.
pub const NONCE_SIGN_OP: &str = "ckbus.nonce_sign";

/// Refusal codes, each an Error frame code on the caller's request.
pub mod code {
    pub const PRINCIPAL_DIRECT: &str = "ckbus_principal_direct";
    pub const GENERATION_NOT_LIVE: &str = "ckbus_generation_not_live";
    pub const CREDENTIAL_SUPERSEDED: &str = super::CREDENTIAL_SUPERSEDED;
    /// Bootstrap has not finished: no box account connection to issue under yet.
    pub const NOT_READY: &str = "ckbus_not_ready";
    /// The supervisor's spawn snapshot could not be read, so nothing is fenced and
    /// nothing is issued.
    pub const SPAWN_SNAPSHOT_UNAVAILABLE: &str = "ckbus_spawn_snapshot_unavailable";
    pub const EPOCH_HIGH_WATER_DAMAGED: &str = "ckbus_epoch_high_water_damaged";
    pub const EPOCH_HIGH_WATER_UNWRITABLE: &str = "ckbus_epoch_high_water_unwritable";
    pub const SIGNING_FAILED: &str = "ckbus_signing_failed";
    pub const GRANT_REFUSED: &str = "ckbus_grant_refused";
    pub const CENSUS_WRITE_FAILED: &str = "ckbus_census_write_failed";
    pub const NAME_REFUSED: &str = "naming-constructor-absent";
    pub const BAD_REQUEST: &str = "ckbus_bad_request";
}

/// A refused request: the Error frame code and a message naming the cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// The live spawn generation of a module, read from the supervisor's spawn snapshot.
#[async_trait]
pub trait LiveGenerations: Send + Sync {
    /// `Ok(None)` when the snapshot shows no live process for the module. `Err` when the
    /// snapshot could not be read, which is never read as "not live".
    async fn live_generation(&self, module_id: &str) -> Result<Option<u64>, String>;
}

/// Everything issuance needs from a finished bootstrap.
#[derive(Clone)]
pub struct Plane {
    pub names: AccountNames,
    /// The box account's id (`A...`), named as `issuer_account` in every user JWT.
    pub account_public: String,
    pub server_url: String,
    /// ck-bus's own box-account connection: durables and census writes go through it.
    pub box_plane: Arc<dyn BoxPlane>,
}

/// Where the current `Plane` comes from: `None` until bootstrap has finished.
pub trait PlaneSource: Send + Sync {
    fn current(&self) -> Option<Plane>;
}

/// A credential ck-bus issued and still holds the key for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issued {
    pub module_id: String,
    pub credential_public: String,
    pub user_jwt_id: String,
    pub spawn_generation: u64,
    pub credential_epoch: u64,
}

/// The answer to `ckbus.credential`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialAnswer {
    pub jwt: String,
    pub acct: String,
    pub account_public: String,
    pub inbox_prefix: String,
    pub server_url: String,
    pub issued: Issued,
}

impl CredentialAnswer {
    pub fn to_json(&self) -> Value {
        json!({
            "jwt": self.jwt,
            "acct": self.acct,
            "account_public": self.account_public,
            "inbox_prefix": self.inbox_prefix,
            "server_url": self.server_url,
            "credential_public": self.issued.credential_public,
            "user_jwt_id": self.issued.user_jwt_id,
            "spawn_generation": self.issued.spawn_generation,
            "credential_epoch": self.issued.credential_epoch,
        })
    }
}

/// The step boundaries of one issue, for the crash controls: a test stops the act right
/// after a step, exactly as a process death there would leave it.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopAfter {
    HighWater,
    Signing,
    Census,
}

pub struct Issuance {
    credentials: Arc<Credentials>,
    high_water: HighWater,
    spawn: Arc<dyn LiveGenerations>,
    plane: Arc<dyn PlaneSource>,
    current: Mutex<HashMap<String, Issued>>,
    /// Per-module serialization, so two concurrent requests for one module never pick
    /// the same epoch or race their census writes.
    module_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// The last high-water damage seen, which holds health down until the operator
    /// repairs the file.
    damage: Mutex<Option<HighWaterRefusal>>,
    #[cfg(test)]
    pub crash_after: Mutex<Option<StopAfter>>,
}

impl Issuance {
    pub fn new(
        credentials: Arc<Credentials>,
        store_root: &std::path::Path,
        spawn: Arc<dyn LiveGenerations>,
        plane: Arc<dyn PlaneSource>,
    ) -> Self {
        let high_water = HighWater::new(store_root);
        high_water.remove_stale_tmp();
        Self {
            credentials,
            high_water,
            spawn,
            plane,
            current: Mutex::new(HashMap::new()),
            module_locks: Mutex::new(HashMap::new()),
            damage: Mutex::new(None),
            #[cfg(test)]
            crash_after: Mutex::new(None),
        }
    }

    /// Where the finished bootstrap's plane is read from, shared with the membership ops.
    pub fn plane_source(&self) -> Arc<dyn PlaneSource> {
        self.plane.clone()
    }

    /// The credential currently held for a module, if any.
    pub fn current(&self, module_id: &str) -> Option<Issued> {
        lock(&self.current).get(module_id).cloned()
    }

    /// The high-water damage that holds health down, if any.
    pub fn damage(&self) -> Option<HighWaterRefusal> {
        lock(&self.damage).clone()
    }

    #[cfg(test)]
    fn crashed_at(&self, boundary: StopAfter) -> bool {
        *lock(&self.crash_after) == Some(boundary)
    }

    async fn live_generation(&self, module_id: &str) -> Result<u64, Refusal> {
        match self.spawn.live_generation(module_id).await {
            Ok(Some(generation)) => Ok(generation),
            Ok(None) => Err(Refusal::new(
                code::GENERATION_NOT_LIVE,
                format!("the spawn snapshot shows no live generation for {module_id}"),
            )),
            Err(error) => Err(Refusal::new(
                code::SPAWN_SNAPSHOT_UNAVAILABLE,
                format!("the spawn snapshot could not be read: {error}"),
            )),
        }
    }

    fn module_lock(&self, module_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        lock(&self.module_locks)
            .entry(module_id.to_string())
            .or_default()
            .clone()
    }

    /// `ckbus.credential` for the attested `module_id`: the six-step act.
    pub async fn issue(&self, module_id: &str) -> Result<CredentialAnswer, Refusal> {
        let names_check = AccountNames::census_key(module_id)
            .map_err(|error| Refusal::new(code::NAME_REFUSED, error.to_string()))?;
        let _serialized = self.module_lock(module_id).lock_owned().await;

        // 1. Fence: the generation comes from the supervisor, never from the caller.
        let generation = self.live_generation(module_id).await?;

        let plane = self.plane.current().ok_or_else(|| {
            Refusal::new(
                code::NOT_READY,
                "bootstrap has not finished; no box account connection yet",
            )
        })?;
        let census_subject = plane
            .names
            .census_subject(&names_check)
            .map_err(|error| Refusal::new(code::NAME_REFUSED, error.to_string()))?;

        // 2. The high-water entry is durable before anything is signed at its epoch.
        let epoch = match self.high_water.advance(module_id, generation) {
            Ok(epoch) => epoch,
            Err(refusal) => {
                let code = if refusal.is_damage() {
                    *lock(&self.damage) = Some(refusal.clone());
                    code::EPOCH_HIGH_WATER_DAMAGED
                } else {
                    code::EPOCH_HIGH_WATER_UNWRITABLE
                };
                log_event(
                    "ckbus.issuance.refused",
                    json!({
                        "module_id": module_id,
                        "spawn_generation": generation,
                        "code": code,
                        "path": refusal.path().display().to_string(),
                        "reason": refusal.to_string(),
                    }),
                );
                return Err(Refusal::new(code, refusal.to_string()));
            }
        };
        #[cfg(test)]
        if self.crashed_at(StopAfter::HighWater) {
            return Err(Refusal::new(
                "test_crash",
                "stopped after the high-water fsync",
            ));
        }

        // 3. A fresh user key in memory; the box account root signs its JWT.
        let custody = &self.credentials.custody;
        let user_public = custody.generate_user();
        let identities: Vec<String> = Vec::new();
        let rooms: Vec<String> = Vec::new();
        let signed = match self.sign(&plane, module_id, &user_public, &rooms).await
        {
            Ok(signed) => signed,
            Err(refusal) => {
                custody.forget(&user_public);
                return Err(refusal);
            }
        };
        #[cfg(test)]
        if self.crashed_at(StopAfter::Signing) {
            custody.forget(&user_public);
            return Err(Refusal::new("test_crash", "stopped after signing"));
        }

        // 4. Retired: agent durables are prefrontal's, bound through the membership ops.

        // 5. The census key, overwritten with this (generation, epoch).
        let value = CensusValue {
            credential_public: user_public.clone(),
            user_jwt_id: signed.jti.clone(),
            spawn_generation: generation,
            credential_epoch: epoch,
            identities: identities.clone(),
            rooms: rooms.clone(),
        };
        if let Err(error) = plane
            .box_plane
            .census_put(&census_subject, value.to_bytes())
            .await
        {
            custody.forget(&user_public);
            return Err(Refusal::new(code::CENSUS_WRITE_FAILED, error.message));
        }
        #[cfg(test)]
        if self.crashed_at(StopAfter::Census) {
            custody.forget(&user_public);
            return Err(Refusal::new("test_crash", "stopped after the census write"));
        }

        let issued = Issued {
            module_id: module_id.to_string(),
            credential_public: user_public.clone(),
            user_jwt_id: signed.jti.clone(),
            spawn_generation: generation,
            credential_epoch: epoch,
        };
        let previous = lock(&self.current).insert(module_id.to_string(), issued.clone());
        if let Some(previous) = previous {
            self.supersede(previous);
        }
        if let Some(rotated_from) = &signed.rotated_from {
            log_event(
                "ckbus.issuance.root_rotated",
                json!({"previous_key_id": rotated_from, "key_id": signed.root_key_id}),
            );
        }
        log_event(
            "ckbus.issuance.issued",
            json!({
                "module_id": module_id,
                "credential_public": user_public,
                "user_jwt_id": signed.jti,
                "spawn_generation": generation,
                "credential_epoch": epoch,
            }),
        );

        // 6. The answer.
        Ok(CredentialAnswer {
            jwt: signed.jwt,
            acct: plane.names.account().to_string(),
            account_public: plane.account_public.clone(),
            inbox_prefix: format!("_INBOX.{user_public}"),
            server_url: plane.server_url.clone(),
            issued,
        })
    }

    async fn sign(
        &self,
        plane: &Plane,
        module_id: &str,
        user_public: &str,
        rooms: &[String],
    ) -> Result<SignedUserJwt, Refusal> {
        let bound_rooms: Vec<&str> = rooms.iter().map(String::as_str).collect();
        let grant: Grant =
            grants::issued_grant(&plane.names, module_id, user_public, &bound_rooms)
                .map_err(|refusal| Refusal::new(code::GRANT_REFUSED, refusal.to_string()))?;
        let root_id = RootCredential::BoxAccount
            .credential_id()
            .map_err(|error| Refusal::new(code::NAME_REFUSED, error.to_string()))?;
        sign_user_jwt(
            self.credentials.vault.as_ref(),
            &self.credentials.key_ids,
            &UserJwtRequest {
                root_credential_id: &root_id,
                user_public,
                issuer_account: Some(&plane.account_public),
                name: module_id,
                issued_at: unix_now(),
                grant: &grant,
            },
        )
        .await
        .map_err(|error| Refusal::new(code::SIGNING_FAILED, error.to_string()))
    }

    /// Drops a superseded key from memory. Nothing is queued: the revocation area finds
    /// the superseded user in the census entry it read before this issue.
    fn supersede(&self, previous: Issued) {
        self.credentials.custody.forget(&previous.credential_public);
        log_event(
            "ckbus.issuance.superseded",
            json!({
                "module_id": previous.module_id,
                "user_public": previous.credential_public,
                "user_jwt_id": previous.user_jwt_id,
                "spawn_generation": previous.spawn_generation,
                "credential_epoch": previous.credential_epoch,
            }),
        );
    }

    /// `ckbus.nonce_sign` for the attested `module_id`: signs with the module's current
    /// key, provided its generation is still the live one. `credential_public`, when the
    /// caller names the key it is connecting as, must be that current key: a caller
    /// reconnecting under a credential ck-bus no longer holds (issued by an earlier
    /// process, or since replaced) is told `ckbus_credential_superseded` and refetches,
    /// rather than getting a signature the server would refuse.
    pub async fn sign_nonce(
        &self,
        module_id: &str,
        credential_public: Option<&str>,
        nonce: &[u8],
    ) -> Result<Vec<u8>, Refusal> {
        let generation = self.live_generation(module_id).await?;
        let Some(current) = self.current(module_id) else {
            return Err(Refusal::new(
                code::CREDENTIAL_SUPERSEDED,
                format!(
                    "ck-bus holds no credential for {module_id}; fetch one with {CREDENTIAL_OP}"
                ),
            ));
        };
        if current.spawn_generation != generation {
            return Err(Refusal::new(
                code::GENERATION_NOT_LIVE,
                format!(
                    "the held credential for {module_id} is for generation {}, the live \
                     generation is {generation}",
                    current.spawn_generation
                ),
            ));
        }
        if let Some(named) = credential_public {
            if named != current.credential_public {
                return Err(Refusal::new(
                    code::CREDENTIAL_SUPERSEDED,
                    format!(
                        "{CREDENTIAL_SUPERSEDED}: ck-bus holds no key for {named}; fetch a fresh \
                         credential with {CREDENTIAL_OP}"
                    ),
                ));
            }
        }
        self.credentials
            .custody
            .sign_nonce(&current.credential_public, nonce)
            .map_err(|superseded| Refusal::new(code::CREDENTIAL_SUPERSEDED, superseded.to_string()))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // Every mutation under these locks is a single insert, push or replace, so a panic
    // while one is held cannot leave a half-written value.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
