//! Bootstrap: the machine id and the box account, ck-bus's own users, the census bucket
//! and the five streams.
//!
//! Keys and signatures follow `docs/designs/nats-install-trust-chain.md` (sections 1, 3
//! and 7). The operator root signed the operator JWT and the system account JWT at
//! install; ck-bus never uses it. At boot ck-bus:
//!
//! 1. signs its system-account user with the system account key and connects to `$SYS`;
//! 2. finds the box account `box_<machine id>`: by the id `account.json` records, or,
//!    when that file is absent, by listing the resolver's accounts and matching the
//!    name, adopting what it finds. Only when no box account exists does it generate the
//!    account identity key, once, in memory, record its public half in `account.json`
//!    and drop the seed;
//! 3. builds the box account JWT for that same id (every limit explicit, the box
//!    account root in `signing_keys`, earlier revocations carried over, the previous
//!    incarnation's box users added), has the operator signer sign it, pushes it, and
//!    reads it back through the claims lookup before treating it as applied;
//! 4. signs its box-account user with the box account root, connects, creates the
//!    census bucket and the five streams if absent, and publishes on its sentinel
//!    subject.
//!
//! Later boots re-sign the account JWT for the same id and never re-key: the account id
//! owns the account's JetStream data. A different machine id while a box account for
//! the old one exists is refused, never answered with a second account; the operator
//! decides the migration. The previous incarnation's system-account users are not
//! revoked: their seeds died with that process, so they cannot answer a nonce, and
//! revoking them would mean re-signing the root-signed system account.
//!
//! Every failure answers health down with class `Unavailable` and a cause naming it.
//! A failure that can clear by itself (the vault, the broker) is retried once per
//! sentinel period; one that needs the operator (a damaged `account.json`, a changed
//! machine id, a missing input) is not. The module never exits for either.

pub mod account_jwt;
pub mod config;
pub mod plane;
pub mod store;

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use cortexkit_bus_naming::{shipped_streams, validate_streams, AccountNames};
use nkeys::KeyPair;
use serde_json::{json, Value};
use subc_client_rs::{ModuleHandler, SubcModuleError};
use subc_protocol::{
    manifest::ModuleManifest,
    session::{HealthReport, HealthStatus},
    MachineId,
};

use crate::{
    credentials::{
        issue::{sign_user_jwt, UserJwtRequest},
        nkey::{encode_public, NkeyRole},
        roots::RootCredential,
        vault::VaultError,
        Credentials,
    },
    grants::{self, Grant, GrantRole},
    runtime::{GrantGeneration, OwnUser, SeamResult, SentinelHealth},
};
use account_jwt::{decode_claims, revocations, sign_account_jwt, AccountClaims};
use config::{BrokerConfig, OperatorFacts};
use plane::{apply_account_jwt, ApplyError, BoxPlane, Broker, NatsBroker, SystemPlane};
use store::{AccountFile, AccountRecord, OwnUsers, OwnUsersFile, Store};

/// Causes named in health `metrics.cause` and in the bootstrap log lines.
pub mod cause {
    pub const MACHINE_ID_ABSENT: &str = "machine-id-absent";
    pub const MACHINE_ID_CHANGED: &str = "machine-id-changed";
    pub const ACCOUNT_JSON_DAMAGED: &str = "account-json-damaged";
    pub const BROKER_CONFIG_ABSENT: &str = "broker-config-absent";
    pub const OPERATOR_JWT_MISMATCH: &str = "operator-jwt-mismatch";
    pub const NAMING_CONSTRUCTOR_ABSENT: &str = "naming-constructor-absent";
    pub const ROOT_KEY_UNREACHABLE: &str = "root-key-unreachable";
    pub const SYSACCOUNT_ABSENT: &str = "sysaccount-absent";
    pub const STORE_WRITE_FAILED: &str = "store-write-failed";
    pub const BROKER_UNAVAILABLE: &str = "broker-unavailable";
    pub const ACCOUNT_NAME_CONFLICT: &str = "account-name-conflict";
    pub const CLAIMS_READBACK_MISMATCH: &str = "claims-readback-mismatch";
    pub const BOX_USER_UNISSUABLE: &str = "box-user-unissuable";
    pub const STREAMS_UNAVAILABLE: &str = "streams-unavailable";
    /// Bootstrap finished; the sentinel, which alone may report the bus up, has not
    /// landed yet.
    pub const SENTINEL_NOT_LANDED: &str = "sentinel-not-landed";
}

/// Why a boot attempt stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub cause: &'static str,
    pub message: String,
    /// Whether the attempt is repeated on the next sentinel period.
    pub retry: bool,
}

impl Failure {
    fn retry(cause: &'static str, message: impl Into<String>) -> Self {
        Self {
            cause,
            message: message.into(),
            retry: true,
        }
    }

    fn stop(cause: &'static str, message: impl Into<String>) -> Self {
        Self {
            cause,
            message: message.into(),
            retry: false,
        }
    }
}

/// A finished bootstrap. The two connections are held for the life of the process.
pub struct Ready {
    pub account: AccountRecord,
    pub system_user: String,
    pub box_user: String,
    pub system: Arc<dyn SystemPlane>,
    pub box_plane: Arc<dyn BoxPlane>,
}

/// Everything a boot attempt needs besides the broker.
pub struct BootDeps {
    pub credentials: Arc<Credentials>,
    pub grants: Arc<dyn GrantGeneration>,
    pub store: Store,
    pub incarnation: String,
}

/// The broker inputs: the environment as read, and the broker built from it.
pub struct BrokerInputs<'a> {
    pub config: &'a Result<BrokerConfig, String>,
    pub broker: &'a dyn Broker,
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

fn root_id(root: RootCredential) -> Result<String, Failure> {
    root.credential_id()
        .map_err(|refusal| Failure::stop(cause::NAMING_CONSTRUCTOR_ABSENT, refusal.to_string()))
}

fn vault_failure(cause: &'static str, error: &VaultError) -> Failure {
    Failure::retry(cause, error.to_string())
}

fn own_grant(
    deps: &BootDeps,
    user: OwnUser,
    names: &AccountNames,
    user_public: &str,
) -> Result<Grant, String> {
    let role = match user {
        OwnUser::BusModule => GrantRole::BusModule,
        OwnUser::SystemAccount => GrantRole::SystemAccount,
    };
    let generated = deps
        .grants
        .own_user_grant(user, names, user_public)
        .map_err(|error| error.to_string())?;
    Grant::from_generated(role, names, &generated).map_err(|error| error.to_string())
}

/// One boot attempt. Nothing is created before the machine id, `account.json` and the
/// broker inputs have all been read and accepted.
pub async fn boot(
    machine_id: Option<&MachineId>,
    inputs: BrokerInputs<'_>,
    deps: &BootDeps,
) -> Result<Ready, Failure> {
    let Some(machine_id) = machine_id else {
        return Err(Failure::stop(
            cause::MACHINE_ID_ABSENT,
            "the daemon's HELLO_ACK carried no machine id: it predates the machine id, so \
             ck-bus creates and issues nothing",
        ));
    };
    let acct = format!("box_{}", machine_id.as_str());
    let names = grants::derive_account(&acct)
        .map_err(|error| Failure::stop(cause::NAMING_CONSTRUCTOR_ABSENT, error.to_string()))?;

    deps.store.remove_stale_tmp();
    let recorded = match deps.store.read_account() {
        AccountFile::Damaged { path, reason } => {
            return Err(Failure::stop(
                cause::ACCOUNT_JSON_DAMAGED,
                format!(
                    "{} is damaged ({reason}); it is left as it is",
                    path.display()
                ),
            ))
        }
        AccountFile::Present(record) if record.machine_id != machine_id.as_str() => {
            return Err(machine_id_changed(
                &record.machine_id,
                machine_id.as_str(),
                &format!("{} ({})", record.acct, record.account_public),
            ))
        }
        AccountFile::Present(record) => Some(record),
        AccountFile::Absent => None,
    };

    let config = inputs
        .config
        .as_ref()
        .map_err(|error| Failure::stop(cause::BROKER_CONFIG_ABSENT, error.clone()))?;
    let operator = config
        .operator_facts()
        .map_err(|error| Failure::stop(cause::OPERATOR_JWT_MISMATCH, error))?;

    let vault = deps.credentials.vault.as_ref();
    let signer_id = root_id(RootCredential::OperatorSigner)?;
    let system_root_id = root_id(RootCredential::SystemAccount)?;
    let box_root_id = root_id(RootCredential::BoxAccount)?;

    let signer = vault
        .public_key(&signer_id)
        .await
        .map_err(|error| vault_failure(cause::ROOT_KEY_UNREACHABLE, &error))?;
    check_signer_listed(
        &operator,
        &encode_public(NkeyRole::Operator, &signer.public),
    )?;

    // The system user first: without it no push, and so no revocation, is possible.
    let previous = deps.store.read_own_users();
    let custody = &deps.credentials.custody;
    let system_user = custody.generate_user();
    let system_grant = own_grant(deps, OwnUser::SystemAccount, &names, &system_user)
        .map_err(|error| Failure::stop(cause::SYSACCOUNT_ABSENT, error))?;
    let system_jwt = sign_user_jwt(
        vault,
        &deps.credentials.key_ids,
        &UserJwtRequest {
            root_credential_id: &system_root_id,
            user_public: &system_user,
            issuer_account: Some(&config.system_account),
            name: "ckbus-system",
            issued_at: unix_now(),
            grant: &system_grant,
        },
    )
    .await
    .map_err(|error| Failure::retry(cause::SYSACCOUNT_ABSENT, error.to_string()))?;

    // Every box user an earlier process (or an earlier attempt of this one) recorded
    // is revoked by this boot's account update.
    let to_revoke: BTreeSet<String> = match &previous {
        OwnUsersFile::Present(users) => users
            .box_users
            .iter()
            .chain(&users.pending_revocation)
            .cloned()
            .collect(),
        OwnUsersFile::Absent | OwnUsersFile::Damaged { .. } => BTreeSet::new(),
    };
    if let OwnUsersFile::Damaged { path, reason } = &previous {
        log_event(
            "ckbus.bootstrap.own_users_damaged",
            json!({
                "path": path.display().to_string(),
                "reason": reason,
                "residual": "the previous incarnation's users are not revoked by name",
            }),
        );
    }
    let record_users = |users: &OwnUsers| -> Result<(), Failure> {
        // A damaged file is left untouched and named, never overwritten.
        if matches!(previous, OwnUsersFile::Damaged { .. }) {
            return Ok(());
        }
        deps.store.write_own_users(users).map_err(|error| {
            Failure::retry(
                cause::STORE_WRITE_FAILED,
                format!("{}: {error}", deps.store.own_users_path().display()),
            )
        })
    };
    record_users(&OwnUsers {
        box_users: Vec::new(),
        system_users: vec![system_user.clone()],
        pending_revocation: to_revoke.iter().cloned().collect(),
    })?;

    let system = inputs
        .broker
        .connect_system(&system_jwt.jwt, &system_user)
        .await
        .map_err(|error| Failure::retry(cause::SYSACCOUNT_ABSENT, error.to_string()))?;

    let (account, existing) =
        resolve_account(system.as_ref(), recorded, machine_id, &names, config, deps).await?;

    let box_root = vault
        .public_key(&box_root_id)
        .await
        .map_err(|error| vault_failure(cause::ROOT_KEY_UNREACHABLE, &error))?;
    let now = unix_now();
    let mut revoked = existing
        .as_ref()
        .map(revocations)
        .unwrap_or_default();
    for user in &to_revoke {
        revoked.entry(user.clone()).or_insert(now);
    }
    // The server keeps the newer of two account JWTs by `iat`, so an update issued in
    // the same second as the one it replaces is dated one second later.
    let previous_iat = existing
        .as_ref()
        .and_then(|claims| claims["iat"].as_i64())
        .unwrap_or_default();
    let claims = AccountClaims {
        account_public: account.account_public.clone(),
        name: account.acct.clone(),
        signing_keys: vec![encode_public(NkeyRole::Account, &box_root.public)],
        revocations: revoked,
        issued_at: now.max(previous_iat + 1),
    };
    let account_jwt = sign_account_jwt(vault, &deps.credentials.key_ids, &signer_id, &claims)
        .await
        .map_err(|error| Failure::retry(cause::ROOT_KEY_UNREACHABLE, error.to_string()))?;
    apply_account_jwt(system.as_ref(), &account.account_public, &account_jwt)
        .await
        .map_err(|error| match error {
            ApplyError::ReadBackMismatch { .. } => {
                Failure::retry(cause::CLAIMS_READBACK_MISMATCH, error.to_string())
            }
            ApplyError::Plane(error) => Failure::retry(cause::BROKER_UNAVAILABLE, error.message),
        })?;

    let box_user = custody.generate_user();
    let box_grant = own_grant(deps, OwnUser::BusModule, &names, &box_user)
        .map_err(|error| Failure::stop(cause::BOX_USER_UNISSUABLE, error))?;
    let box_jwt = sign_user_jwt(
        vault,
        &deps.credentials.key_ids,
        &UserJwtRequest {
            root_credential_id: &box_root_id,
            user_public: &box_user,
            issuer_account: Some(&account.account_public),
            name: "ckbus-box",
            issued_at: unix_now(),
            grant: &box_grant,
        },
    )
    .await
    .map_err(|error| Failure::retry(cause::BOX_USER_UNISSUABLE, error.to_string()))?;
    // The revocation is read back, so nothing stays pending.
    record_users(&OwnUsers {
        box_users: vec![box_user.clone()],
        system_users: vec![system_user.clone()],
        pending_revocation: Vec::new(),
    })?;
    let box_plane = inputs
        .broker
        .connect_box(&box_jwt.jwt, &box_user)
        .await
        .map_err(|error| Failure::retry(cause::BOX_USER_UNISSUABLE, error.to_string()))?;

    let streams = shipped_streams(&names);
    validate_streams(&streams)
        .map_err(|error| Failure::stop(cause::STREAMS_UNAVAILABLE, error.to_string()))?;
    box_plane
        .ensure_census(&names)
        .await
        .map_err(|error| Failure::retry(cause::STREAMS_UNAVAILABLE, error.message))?;
    for spec in &streams {
        box_plane
            .ensure_stream(spec)
            .await
            .map_err(|error| Failure::retry(cause::STREAMS_UNAVAILABLE, error.message))?;
    }
    // ck-bus's own census key needs the census key grammar, which the naming crate
    // does not construct at the pinned revision; the key is not written until it does.
    log_event(
        "ckbus.bootstrap.census_key_skipped",
        json!({
            "gate": cause::NAMING_CONSTRUCTOR_ABSENT,
            "constructor": "census key grammar",
        }),
    );
    box_plane
        .publish(
            &names.sentinel_ping(),
            serde_json::to_vec(&json!({
                "event": "ckbus.bootstrap.ready",
                "incarnation": deps.incarnation,
            }))
            .unwrap_or_default(),
        )
        .await
        .map_err(|error| Failure::retry(cause::BROKER_UNAVAILABLE, error.message))?;

    Ok(Ready {
        account,
        system_user,
        box_user,
        system,
        box_plane,
    })
}

fn machine_id_changed(recorded: &str, current: &str, existing: &str) -> Failure {
    Failure::stop(
        cause::MACHINE_ID_CHANGED,
        format!(
            "the machine id changed from {recorded} to {current} while the box account \
             {existing} exists; ck-bus creates no second box account, and the operator \
             decides the migration"
        ),
    )
}

/// Design 6.8: the operator signer ck-bus signs with must be one the operator JWT lists,
/// or every account JWT it signs would be untrusted.
fn check_signer_listed(operator: &OperatorFacts, signer: &str) -> Result<(), Failure> {
    if operator.signing_keys.iter().any(|key| key == signer) {
        return Ok(());
    }
    Err(Failure::stop(
        cause::OPERATOR_JWT_MISMATCH,
        format!(
            "the vault's operator signer {signer} is not among the operator JWT's signing \
             keys {:?}",
            operator.signing_keys
        ),
    ))
}

/// Finds the box account for `names`, returning its record and its current claims.
/// With `account.json` absent, the resolver's accounts are listed and matched by name
/// before anything is created, so a lost state file never makes a second account.
async fn resolve_account(
    system: &dyn SystemPlane,
    recorded: Option<AccountRecord>,
    machine_id: &MachineId,
    names: &AccountNames,
    config: &BrokerConfig,
    deps: &BootDeps,
) -> Result<(AccountRecord, Option<Value>), Failure> {
    let broker =
        |error: plane::PlaneError| Failure::retry(cause::BROKER_UNAVAILABLE, error.message);
    if let Some(record) = recorded {
        let existing = system
            .lookup(&record.account_public)
            .await
            .map_err(broker)?
            .and_then(|jwt| decode_claims(&jwt));
        if let Some(claims) = &existing {
            let name = claims["name"].as_str().unwrap_or_default();
            if name != record.acct {
                return Err(Failure::stop(
                    cause::ACCOUNT_NAME_CONFLICT,
                    format!(
                        "account.json names {} for {}, the server's JWT names {name:?}",
                        record.acct, record.account_public
                    ),
                ));
            }
        }
        return Ok((record, existing));
    }

    let mut matching = Vec::new();
    let mut other_boxes = Vec::new();
    for id in system.list_accounts().await.map_err(broker)? {
        if id == config.system_account {
            continue;
        }
        let Some(claims) = system
            .lookup(&id)
            .await
            .map_err(broker)?
            .and_then(|jwt| decode_claims(&jwt))
        else {
            continue;
        };
        let name = claims["name"].as_str().unwrap_or_default().to_string();
        if name == names.account() {
            matching.push((id, claims));
        } else if name.starts_with("box_") {
            other_boxes.push(format!("{name} ({id})"));
        }
    }
    if !other_boxes.is_empty() {
        let recorded_ids = other_boxes
            .iter()
            .map(|entry| entry.trim_start_matches("box_"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(machine_id_changed(
            &recorded_ids,
            machine_id.as_str(),
            &other_boxes.join(", "),
        ));
    }
    if matching.len() > 1 {
        return Err(Failure::stop(
            cause::ACCOUNT_NAME_CONFLICT,
            format!(
                "the server holds {} accounts named {}: {:?}",
                matching.len(),
                names.account(),
                matching.iter().map(|(id, _)| id).collect::<Vec<_>>()
            ),
        ));
    }
    let (account_public, existing) = match matching.pop() {
        Some((id, claims)) => (id, Some(claims)),
        None => {
            // The account identity: generated once, here, in memory. Only its public
            // half is kept; nobody holds the seed, because user JWTs are signed by the
            // box account root listed in `signing_keys`.
            let identity = KeyPair::new_account();
            (identity.public_key(), None)
        }
    };
    let record = AccountRecord {
        machine_id: machine_id.as_str().to_string(),
        acct: names.account().to_string(),
        account_public,
    };
    // Recorded before any push, so a crash after the push still finds the id.
    deps.store.write_account(&record).map_err(|error| {
        Failure::retry(
            cause::STORE_WRITE_FAILED,
            format!("{}: {error}", deps.store.account_path().display()),
        )
    })?;
    log_event(
        "ckbus.bootstrap.account_recorded",
        json!({
            "acct": record.acct,
            "account_public": record.account_public,
            "adopted": existing.is_some(),
        }),
    );
    Ok((record, existing))
}

/// One structured stderr line. `at_ms` lets a reader measure the retry period from the
/// log alone.
fn log_event(event: &str, fields: Value) {
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

/// What `health.check` answers while bootstrap runs, fails or finishes.
#[derive(Debug, Clone)]
enum BootState {
    Starting,
    Down(Failure),
    Ready {
        acct: String,
        account_public: String,
    },
}

/// The health answer. Until the sentinel lands, bootstrap alone never reports the bus
/// up: a finished bootstrap still answers down, naming `sentinel-not-landed`.
#[derive(Clone)]
pub struct BootstrapHealth {
    state: Arc<Mutex<BootState>>,
}

impl BootstrapHealth {
    fn set(&self, state: BootState) {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = state;
    }
}

#[async_trait]
impl SentinelHealth for BootstrapHealth {
    async fn report_health(&self) -> SeamResult<HealthReport> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let metrics = match state {
            BootState::Starting => json!({
                "class": "Unavailable",
                "bootstrap": "starting",
            }),
            BootState::Down(failure) => json!({
                "class": "Unavailable",
                "bootstrap": "down",
                "cause": failure.cause,
                "message": failure.message,
            }),
            BootState::Ready {
                acct,
                account_public,
            } => json!({
                "class": "Unavailable",
                "bootstrap": "ready",
                "cause": cause::SENTINEL_NOT_LANDED,
                "acct": acct,
                "account_public": account_public,
            }),
        };
        Ok(HealthReport {
            status: HealthStatus::Failing,
            detail: Some("bus.health.down".to_string()),
            metrics: Some(metrics),
        })
    }
}

/// The bootstrap area as wired at start.
pub struct Bootstrap {
    deps: BootDeps,
    period: Duration,
    health: BootstrapHealth,
}

impl Bootstrap {
    pub fn new(deps: BootDeps, period: Duration) -> Self {
        Self {
            deps,
            period,
            health: BootstrapHealth {
                state: Arc::new(Mutex::new(BootState::Starting)),
            },
        }
    }

    pub fn health(&self) -> Arc<dyn SentinelHealth> {
        Arc::new(self.health.clone())
    }

    /// Registers the module and, once HELLO_ACK names the machine, runs bootstrap
    /// beside the serving connection.
    pub async fn serve<H: ModuleHandler>(
        self,
        manifest: ModuleManifest,
        handler: H,
    ) -> Result<(), SubcModuleError> {
        let connection_file = crate::credentials::vault::subc_arg(std::env::args_os())
            .ok_or(SubcModuleError::MissingSubcArg)?;
        let (handle, serving) =
            subc_client_rs::serve_with_handle(&connection_file, manifest, handler).await?;
        let machine_id = handle.machine_id().cloned();
        let running = tokio::spawn(self.run(machine_id));
        let served = serving.await;
        running.abort();
        served
    }

    async fn run(self, machine_id: Option<MachineId>) {
        let config = BrokerConfig::from_env();
        let broker_url = config.as_ref().map(|config| config.url.clone()).ok();
        let broker = NatsBroker::new(
            broker_url.unwrap_or_default(),
            self.deps.credentials.clone(),
        );
        let mut attempt = 0u64;
        loop {
            attempt += 1;
            let outcome = boot(
                machine_id.as_ref(),
                BrokerInputs {
                    config: &config,
                    broker: &broker,
                },
                &self.deps,
            )
            .await;
            match outcome {
                Ok(ready) => {
                    log_event(
                        "ckbus.bootstrap.ready",
                        json!({
                            "attempt": attempt,
                            "acct": ready.account.acct,
                            "account_public": ready.account.account_public,
                            "machine_id": ready.account.machine_id,
                            "system_user": ready.system_user,
                            "box_user": ready.box_user,
                            "incarnation": self.deps.incarnation,
                        }),
                    );
                    self.health.set(BootState::Ready {
                        acct: ready.account.acct.clone(),
                        account_public: ready.account.account_public.clone(),
                    });
                    // The connections stay open for as long as the process serves.
                    let _held = ready;
                    std::future::pending::<()>().await;
                }
                Err(failure) => {
                    log_event(
                        "ckbus.bootstrap.down",
                        json!({
                            "attempt": attempt,
                            "cause": failure.cause,
                            "message": failure.message,
                            "retry": failure.retry,
                            "period_ms": self.period.as_millis() as u64,
                        }),
                    );
                    let retry = failure.retry;
                    self.health.set(BootState::Down(failure));
                    if !retry {
                        return;
                    }
                    tokio::time::sleep(self.period).await;
                }
            }
        }
    }
}
