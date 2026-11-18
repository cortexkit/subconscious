//! Issuance: a supervised participant asks ck-bus for its bus credential.
//!
//! ck-bus issues and renews box and hub credentials and signs their connect nonces.
//! Every user JWT expires 15 minutes after it is signed;
//! renewal re-signs the same key with a fresh `exp`, and the holder renews before the old
//! one lapses. All credential ops
//! authorize by the principal the daemon stamped on the caller's route at bind time
//! and by nothing the caller writes in its request: `Principal::Reserved { module_id }`
//! (the caller presented its launch nonce) binds the answer to that module id and to the
//! live spawn generation the supervisor's spawn snapshot shows for it. A body field
//! naming another module id is ignored. Any other principal is refused
//! `ckbus_principal_direct`, and a module with no live generation
//! `ckbus_generation_not_live`.
//!
//! The census key is the module's entry in the account's census bucket
//! (`AccountNames::census_key`, value in `census`): it names the credential the module
//! currently holds, and revocation reads it to find the key to revoke. Issuing and
//! writing the census key are one act, in this order:
//! 1. fence against the spawn snapshot's generation;
//! 2. advance and fsync the generation's entry in `epoch_high_water.json`;
//! 3. generate the user key in memory and have the box account root (the vault key
//!    that signs every user JWT in the box account) sign its JWT;
//! 4. (retired: issuance no longer creates agent durables; prefrontal creates them
//!    through `ckbus.agent_durable_bind`, in the membership area);
//! 5. write the census key, including a digest of the signed permission set;
//! 6. answer.
//!
//! A failure (or a crash) before step 5 leaves a signed JWT whose seed is dropped and no
//! census entry, so there is nothing to roll back; the next request issues at a higher
//! epoch. Once an issue completes, the module's previous user is superseded: its key is
//! dropped from memory (so a reconnect under it gets `ckbus_credential_superseded`). Its
//! revocation is recorded durably by the replacement guard (a [`CensusReplacement`]
//! the revocation area installs) after signing and before the census write. The census
//! entry is the only record naming the old key, so recording it first means a crash
//! during the overwrite cannot lose it. If the census write then fails, that old key
//! remains revoked; the caller must fetch a fresh key.
//!
//! The grant names no agent: agent access is account-scoped, so a credential may pull
//! from any agent's durable in the box account and a change in where an agent resides
//! needs no reissue. `grants::issued_grant` picks the grant from the attested module id:
//! the delivery-authority grant for either prefrontal id, the flow-engine grant for
//! `basal`, and the participant grant for every other module. A grant can also name the
//! rooms (the `ck.{acct}.room.{room_id}` subjects) a credential is bound to, but nothing
//! yet tells ck-bus which rooms a module belongs to, so every module is issued with none.

pub use crate::bootstrap::census;
pub mod handler;
pub mod high_water;

use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use cortexkit_bus_naming::{
    federation::{hub_module_grant, is_hub_module, module_deliver_prefix, BoxId},
    AccountNames,
};
use serde_json::{json, Value};

use crate::{
    bootstrap::{
        federation_account,
        plane::{BoxPlane, SystemPlane},
        store::Store,
    },
    credentials::{
        custody::CREDENTIAL_SUPERSEDED,
        issue::{sign_user_jwt, IssueError, SignedUserJwt, UserJwtRequest},
        roots::RootCredential,
        vault::VaultError,
        Credentials,
    },
    grants::{self, Grant, GrantRole},
};
use census::CensusValue;
use high_water::{HighWater, HighWaterRefusal};

/// The op a participant calls to fetch its credential.
pub const CREDENTIAL_OP: &str = "ckbus.credential";
/// The op a participant calls to have its connect nonce signed.
pub const NONCE_SIGN_OP: &str = "ckbus.nonce_sign";
/// The op a participant calls to renew its JWT before `exp` (every JWT expires 15
/// minutes after signing): the same key and epoch, re-signed with a fresh `iat` and `exp`.
pub const CREDENTIAL_RENEW_OP: &str = "ckbus.credential_renew";
pub const HUB_CREDENTIAL_OP: &str = "ckbus.hub_credential";
pub const HUB_CREDENTIAL_RENEW_OP: &str = "ckbus.hub_credential_renew";

/// Refusal codes, each an Error frame code on the caller's request.
pub mod code {
    pub const PRINCIPAL_DIRECT: &str = "ckbus_principal_direct";
    pub const GENERATION_NOT_LIVE: &str = "ckbus_generation_not_live";
    pub const CREDENTIAL_SUPERSEDED: &str = super::CREDENTIAL_SUPERSEDED;
    pub const CREDENTIAL_REVOKED: &str = crate::credentials::custody::CREDENTIAL_REVOKED;
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
    pub const CENSUS_UNAVAILABLE: &str = "ckbus_census_unavailable";
    pub const REVOCATION_UNWRITABLE: &str = "ckbus_revocation_unwritable";
    pub const NAME_REFUSED: &str = "naming-constructor-absent";
    pub const BAD_REQUEST: &str = "ckbus_bad_request";
    pub const HUB_NOT_GRANTED: &str = "hub_not_granted";
    pub const HUB_UNAVAILABLE: &str = "hub_unavailable";
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

pub struct HubPlane {
    pub account_public: String,
    pub box_id: BoxId,
    pub system: Arc<dyn SystemPlane>,
    pub store: Store,
}

#[async_trait]
pub trait HubSource: Send + Sync {
    /// Requires a live hub link and a reachable federation-account signer.
    async fn current(&self) -> Result<HubPlane, Refusal>;

    fn fedout_waiting(&self) -> bool {
        false
    }
}

/// The replacement guard. Before issuance overwrites a module's census entry, `prepare`
/// durably records a revocation of the credential that entry names (`previous`, the
/// module's previous credential), so the old key can still be revoked after the entry
/// that named it is gone, even across a crash.
pub trait CensusReplacement: Send + Sync {
    fn prepare(&self, module_id: &str, previous: &CensusValue) -> Result<(), Refusal>;
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
    /// The JWT's `exp` claim, so the holder schedules its renewal without decoding it.
    pub exp: i64,
    pub acct: String,
    pub account_public: String,
    pub inbox_prefix: String,
    pub server_url: String,
    pub issued: Issued,
    pub box_id: Option<BoxId>,
}

impl CredentialAnswer {
    pub fn to_json(&self) -> Value {
        let mut result = json!({
            "jwt": self.jwt,
            "exp": self.exp,
            "acct": self.acct,
            "account_public": self.account_public,
            "inbox_prefix": self.inbox_prefix,
            "server_url": self.server_url,
            "credential_public": self.issued.credential_public,
            "user_jwt_id": self.issued.user_jwt_id,
            "spawn_generation": self.issued.spawn_generation,
            "credential_epoch": self.issued.credential_epoch,
        });
        if let Some(box_id) = &self.box_id {
            result["box_id"] = json!(box_id.as_str());
            result["domain"] = json!("hub");
        }
        result
    }
}

/// The answer to `ckbus.credential_renew`: the renewed JWT and its `exp`, and the
/// credential it renews, so a renewal that raced a supersede is recognisable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenewAnswer {
    pub jwt: String,
    pub exp: i64,
    /// The renewed token's own `jti`. The census keeps the `user_jwt_id` of the issue.
    pub user_jwt_id: String,
    pub issued: Issued,
}

impl RenewAnswer {
    pub fn to_json(&self) -> Value {
        json!({
            "jwt": self.jwt,
            "exp": self.exp,
            "user_jwt_id": self.user_jwt_id,
            "credential_public": self.issued.credential_public,
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
    hub: Mutex<Option<Arc<dyn HubSource>>>,
    current: Mutex<HashMap<String, Issued>>,
    /// Per-module serialization, so two concurrent requests for one module never pick
    /// the same epoch or race their census writes.
    module_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// The last high-water damage seen, which holds health down until the operator
    /// repairs the file.
    damage: Mutex<Option<HighWaterRefusal>>,
    replacement: Mutex<Option<Arc<dyn CensusReplacement>>>,
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
            hub: Mutex::new(None),
            current: Mutex::new(HashMap::new()),
            module_locks: Mutex::new(HashMap::new()),
            damage: Mutex::new(None),
            replacement: Mutex::new(None),
            #[cfg(test)]
            crash_after: Mutex::new(None),
        }
    }

    /// Where the finished bootstrap's plane is read from, shared with the membership ops.
    pub fn plane_source(&self) -> Arc<dyn PlaneSource> {
        self.plane.clone()
    }

    /// Installs the replacement guard. `revocation::handler::wire` calls this before the
    /// module serves any request: without a guard, issuance overwrites a census entry
    /// without recording the previous credential's revocation, and that credential
    /// would stay usable until its JWT expired.
    pub fn set_replacement_guard(&self, guard: Arc<dyn CensusReplacement>) {
        *lock(&self.replacement) = Some(guard);
    }

    pub fn set_hub_source(&self, hub: Arc<dyn HubSource>) {
        *lock(&self.hub) = Some(hub);
    }

    fn fedout_waiting(&self) -> bool {
        lock(&self.hub)
            .as_ref()
            .is_some_and(|hub| hub.fedout_waiting())
    }

    async fn hub(&self, module_id: &str) -> Result<HubPlane, Refusal> {
        if !is_hub_module(module_id) {
            return Err(Refusal::new(
                code::HUB_NOT_GRANTED,
                format!("{module_id} has no hub grant"),
            ));
        }
        let source = lock(&self.hub)
            .clone()
            .ok_or_else(|| Refusal::new(code::HUB_UNAVAILABLE, "the hub link is not available"))?;
        source.current().await
    }

    /// The credential currently held for a module, if any.
    pub fn current(&self, module_id: &str) -> Option<Issued> {
        lock(&self.current).get(module_id).cloned()
    }

    /// The high-water damage that holds health down, if any.
    pub fn damage(&self) -> Option<HighWaterRefusal> {
        let mut damage = lock(&self.damage);
        if damage
            .as_ref()
            .is_some_and(|damage| self.high_water.damage_is_repaired(damage))
        {
            *damage = None;
        }
        damage.clone()
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
        self.issue_in(module_id, None).await
    }

    pub async fn issue_hub(&self, module_id: &str) -> Result<CredentialAnswer, Refusal> {
        let hub = self.hub(module_id).await?;
        self.issue_in(module_id, Some(hub)).await
    }

    async fn issue_in(
        &self,
        module_id: &str,
        hub: Option<HubPlane>,
    ) -> Result<CredentialAnswer, Refusal> {
        let module_key = AccountNames::census_key(module_id)
            .map_err(|error| Refusal::new(code::NAME_REFUSED, error.to_string()))?;
        let names_check = if hub.is_some() {
            format!("{}{module_key}", census::HUB_PREFIX)
        } else {
            module_key
        };
        let _serialized = self.module_lock(&names_check).lock_owned().await;

        // 1. Fence: the generation comes from the supervisor, never from the caller.
        let generation = self.live_generation(module_id).await?;

        let plane = self.plane.current().ok_or_else(|| {
            Refusal::new(
                code::NOT_READY,
                "bootstrap has not finished; no box account connection yet",
            )
        })?;
        let census_subject = format!("$KV.{}.{names_check}", plane.names.buckets().census);

        // 2. The high-water entry is durable before anything is signed at its epoch.
        let epoch = match self.high_water.advance(&names_check, generation) {
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
        let (signed, grant_digest) = match self
            .sign(&plane, hub.as_ref(), module_id, &user_public)
            .await
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
            grant_digest: Some(grant_digest),
            identities: identities.clone(),
            rooms: rooms.clone(),
        };
        let guard = lock(&self.replacement).clone();
        if let Some(guard) = guard {
            let prepared = async {
                let previous = plane
                    .box_plane
                    .census_get(&plane.names, &names_check)
                    .await
                    .map_err(|error| Refusal::new(code::CENSUS_UNAVAILABLE, error.message))?;
                if let Some(previous) = previous {
                    let previous = CensusValue::parse(&previous.value)
                        .map_err(|reason| Refusal::new(code::CENSUS_UNAVAILABLE, reason))?;
                    guard.prepare(&names_check, &previous)?;
                }
                Ok::<_, Refusal>(())
            }
            .await;
            if let Err(refusal) = prepared {
                custody.forget(&user_public);
                return Err(refusal);
            }
        }
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
        let previous = lock(&self.current).insert(names_check, issued.clone());
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
        let (acct, account_public, inbox_prefix, box_id) = match hub {
            Some(hub) => (
                federation_account::federation_account_name(hub.box_id.as_str()),
                hub.account_public,
                module_deliver_prefix(&hub.box_id, module_id).expect("validated module id"),
                Some(hub.box_id),
            ),
            None => (
                plane.names.account().to_string(),
                plane.account_public,
                format!("_INBOX.{user_public}"),
                None,
            ),
        };
        Ok(CredentialAnswer {
            jwt: signed.jwt,
            exp: signed.exp,
            acct,
            account_public,
            inbox_prefix,
            server_url: plane.server_url.clone(),
            issued,
            box_id,
        })
    }

    async fn sign(
        &self,
        plane: &Plane,
        hub: Option<&HubPlane>,
        module_id: &str,
        user_public: &str,
    ) -> Result<(SignedUserJwt, String), Refusal> {
        let (grant, account, root) = if let Some(hub) = hub {
            let grant = hub_module_grant(&hub.box_id, module_id).ok_or_else(|| {
                Refusal::new(
                    code::HUB_NOT_GRANTED,
                    format!("{module_id} has no hub grant"),
                )
            })?;
            (
                Grant::with_denies(
                    GrantRole::Participant,
                    &federation_account::federation_account_name(hub.box_id.as_str()),
                    grant.publish.into_iter().collect(),
                    grant.subscribe.into_iter().collect(),
                    grant.denies,
                ),
                &hub.account_public,
                RootCredential::FederationAccount,
            )
        } else {
            (
                grants::issued_grant(&plane.names, module_id, user_public, &[])
                    .map_err(|refusal| Refusal::new(code::GRANT_REFUSED, refusal.to_string()))?,
                &plane.account_public,
                RootCredential::BoxAccount,
            )
        };
        let root_id = root
            .credential_id()
            .map_err(|error| Refusal::new(code::NAME_REFUSED, error.to_string()))?;
        let issued_at = unix_now();
        let signed = sign_user_jwt(
            self.credentials.vault.as_ref(),
            &self.credentials.key_ids,
            &UserJwtRequest {
                root_credential_id: &root_id,
                user_public,
                issuer_account: Some(account),
                name: module_id,
                issued_at,
                expires_at: self.credentials.lifetime.expires_at(issued_at),
                grant: &grant,
            },
        )
        .await
        .map_err(|error| {
            let code = if hub.is_some()
                && matches!(
                    error,
                    IssueError::Vault(VaultError::RootKeyUnreachable { .. })
                ) {
                code::HUB_UNAVAILABLE
            } else {
                code::SIGNING_FAILED
            };
            Refusal::new(code, error.to_string())
        })?;
        if let Some(hub) = hub {
            let actual = federation_account::ensure_account(
                hub.system.as_ref(),
                &self.credentials,
                &hub.store,
                hub.box_id.as_str(),
                Some(signed.issuer.clone()),
            )
            .await
            .map_err(|error| Refusal::new(code::HUB_UNAVAILABLE, error))?;
            if actual != *account {
                return Err(Refusal::new(
                    code::HUB_UNAVAILABLE,
                    "the federation account changed while signing",
                ));
            }
        }
        Ok((signed, grant.permission_digest()))
    }

    /// `ckbus.credential_renew` for the attested `module_id` (R16): re-signs the key the
    /// caller names with a fresh `iat` and `exp`, at the same generation and epoch. The
    /// census is not rewritten and nothing is revoked: the caller's connection runs on
    /// its old JWT until nats-server ends it at that JWT's `exp`, and the reconnect uses
    /// the renewed one.
    ///
    /// Refused, by name, so the caller fetches a new credential with `ckbus.credential`
    /// instead: `ckbus_credential_revoked` when this process has recorded the key's
    /// revocation (a revoked key's replacement is always a fresh key), and
    /// `ckbus_credential_superseded` when the key is not the module's current one
    /// (replaced by a later issue, or issued by an earlier ck-bus process).
    pub async fn renew(
        &self,
        module_id: &str,
        credential_public: &str,
    ) -> Result<RenewAnswer, Refusal> {
        self.renew_in(module_id, credential_public, None).await
    }

    pub async fn renew_hub(
        &self,
        module_id: &str,
        credential_public: &str,
    ) -> Result<RenewAnswer, Refusal> {
        let hub = self.hub(module_id).await?;
        self.renew_in(module_id, credential_public, Some(hub)).await
    }

    async fn renew_in(
        &self,
        module_id: &str,
        credential_public: &str,
        hub: Option<HubPlane>,
    ) -> Result<RenewAnswer, Refusal> {
        let (key, op) = if hub.is_some() {
            (
                format!("{}{module_id}", census::HUB_PREFIX),
                HUB_CREDENTIAL_OP,
            )
        } else {
            (module_id.to_owned(), CREDENTIAL_OP)
        };
        let _serialized = self.module_lock(&key).lock_owned().await;
        let generation = self.live_generation(module_id).await?;
        let revoked = || {
            Refusal::new(
                code::CREDENTIAL_REVOKED,
                format!(
                    "{}: {credential_public} is revoked; fetch a fresh credential with \
                     {op}",
                    code::CREDENTIAL_REVOKED
                ),
            )
        };
        if self.credentials.custody.is_revoked(credential_public) {
            return Err(revoked());
        }
        let current = self
            .current(&key)
            .filter(|current| current.credential_public == credential_public)
            .filter(|current| self.credentials.custody.holds(&current.credential_public))
            .ok_or_else(|| {
                Refusal::new(
                    code::CREDENTIAL_SUPERSEDED,
                    format!(
                        "{CREDENTIAL_SUPERSEDED}: {credential_public} is not {module_id}'s \
                         current credential; fetch a fresh one with {op}"
                    ),
                )
            })?;
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
        let plane = self.plane.current().ok_or_else(|| {
            Refusal::new(
                code::NOT_READY,
                "bootstrap has not finished; no box account connection yet",
            )
        })?;
        let (signed, _) = self
            .sign(&plane, hub.as_ref(), module_id, credential_public)
            .await?;
        // Checked again after signing: a revocation recorded while the vault signed has
        // a timestamp no earlier than this JWT's `iat`, so the server would refuse the
        // token anyway, and answering it would only send the caller a dead credential.
        if self.credentials.custody.is_revoked(credential_public) {
            return Err(revoked());
        }
        log_event(
            "ckbus.issuance.renewed",
            json!({
                "module_id": module_id,
                "credential_public": credential_public,
                "user_jwt_id": signed.jti,
                "census_user_jwt_id": current.user_jwt_id,
                "spawn_generation": current.spawn_generation,
                "credential_epoch": current.credential_epoch,
                "exp": signed.exp,
            }),
        );
        Ok(RenewAnswer {
            jwt: signed.jwt,
            exp: signed.exp,
            user_jwt_id: signed.jti,
            issued: current,
        })
    }

    /// Drops a superseded key from memory. The replacement guard already recorded
    /// the predecessor before its census entry was overwritten.
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
        let current = match credential_public {
            Some(public) => self
                .current(module_id)
                .filter(|current| current.credential_public == public)
                .or_else(|| {
                    is_hub_module(module_id)
                        .then(|| self.current(&format!("{}{module_id}", census::HUB_PREFIX)))
                        .flatten()
                        .filter(|current| current.credential_public == public)
                }),
            None => self.current(module_id),
        };
        let Some(current) = current else {
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
