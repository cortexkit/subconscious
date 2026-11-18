//! The hub area's state and the task that drives it.
//!
//! One task owns every decision: it follows callosum's hub row, runs hub setup when the
//! row names this computer as the hub (and again only after a hub key rotates), brings
//! the hub instance's accounts and streams up once the instance answers, follows the
//! paired devices to revoke a removed computer's leaf key, and publishes the listener
//! state `ckbus.hub_server_watch` sends. The ops (`hub_tls_sign`, `hub_identity_read`,
//! `hub_leaf_issue`) read what the task published and never wait on it.

#[cfg(test)]
use std::collections::BTreeMap;
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use nkeys::KeyPair;
use serde_json::{json, Map, Value};
use tokio::{
    sync::{mpsc, watch, Notify},
    time::Instant,
};

use super::{
    cert::{certificate_public_key, self_issued_certificate, spki_sha256_hex},
    conf::{url_port, HubConf, HUB_ADMIN_PORT, HUB_LEAF_PORT},
    devices::{machine_id_for, parse_devices, Devices},
    grant::leaf_permissions,
    issue_code,
    jwt::{
        as_account_key, hub_account_claims, new_account_identity, operator_claims, sign_claims,
        system_account_claims, UserJwt, LEAF_JWT_LIFETIME_SECS,
    },
    rate::TokenBucket,
    record::{self, HubRecord, Issued, RecordFile, Removed},
    seams::{HubServer, HubStream, OwnMachine, PairedDevices},
    tls_code,
};
use crate::{
    credentials::{
        nkey::NkeyRole, roots::RootCredential, vault::VaultSigning, wire::VaultPublicKey,
    },
    federation::leaf::{
        area::{follow_hub, log_event},
        credential::Backoff,
        hub::HubView,
        seams::{Callosum, LocalHub, LocalHubIdentity},
    },
};

/// How often things happen. Production uses `HubTiming::default()`; tests shorten it.
#[derive(Debug, Clone)]
pub struct HubTiming {
    /// `callosum.hub_read` and `callosum.paired_devices_read` cadence while their
    /// watches are down.
    pub read_period: Duration,
    /// How often the hub keys' public halves are re-read, to notice a rotation.
    pub key_poll: Duration,
    /// How often the hub's leaf report is read for health.
    pub leaf_poll: Duration,
    /// The driving task's idle wake-up.
    pub tick: Duration,
    /// The bound on one `credential.sign` for a TLS handshake, inside the wrapper's 4 s
    /// bound on `ckbus.hub_tls_sign`, so the wrapper always gets this module's reason.
    pub sign_timeout: Duration,
}

impl Default for HubTiming {
    fn default() -> Self {
        Self {
            read_period: Duration::from_secs(60),
            key_poll: Duration::from_secs(60),
            leaf_poll: Duration::from_secs(10),
            tick: Duration::from_secs(1),
            sign_timeout: Duration::from_millis(3_500),
        }
    }
}

/// Where the hub's files live.
#[derive(Debug, Clone)]
pub struct HubFiles {
    /// ck-bus's store, which holds `hub.json`.
    pub store_root: PathBuf,
    /// `<cortexkit data dir>/nats-hub`: `server.conf`, `operator.jwt`, the resolver
    /// directory and the JetStream store.
    pub hub_dir: PathBuf,
}

impl HubFiles {
    pub fn server_conf(&self) -> PathBuf {
        self.hub_dir.join("server.conf")
    }

    pub fn operator_jwt(&self) -> PathBuf {
        self.hub_dir.join("operator.jwt")
    }

    pub fn jwt_dir(&self) -> PathBuf {
        self.hub_dir.join("jwt")
    }

    pub fn js_dir(&self) -> PathBuf {
        self.hub_dir.join("js")
    }
}

/// Everything outside the area.
pub struct HubDeps {
    pub callosum: Arc<dyn Callosum>,
    pub devices: Arc<dyn PairedDevices>,
    pub vault: Arc<dyn VaultSigning>,
    pub server: Arc<dyn HubServer>,
    pub own: Arc<dyn OwnMachine>,
    pub files: HubFiles,
    /// The local server's client URL, so setup can refuse an admin port it already uses.
    pub local_server_url: Option<String>,
}

/// What `ckbus.hub_server_watch` sends: the engine's `state` without its `v`, `type` and
/// (always empty) remotes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubServerState {
    pub generation: u64,
    pub listener: Option<Vec<u8>>,
}

impl HubServerState {
    pub fn to_json(&self) -> Value {
        let mut state = json!({ "generation": self.generation });
        if let Some(cert) = &self.listener {
            state["hub_listener"] = json!({
                "port": HUB_LEAF_PORT,
                "cert_der_b64": URL_SAFE_NO_PAD.encode(cert),
            });
        }
        state
    }
}

/// Why an op produced no answer. The code is the short reason a caller logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Hub setup as health reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SetupState {
    NotHub,
    SettingUp,
    Ready,
    Failed,
}

impl SetupState {
    fn name(&self) -> &'static str {
        match self {
            Self::NotHub => "not_hub",
            Self::SettingUp => "setting_up",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Default)]
struct Retry {
    at: Option<Instant>,
    backoff: Backoff,
}

impl Retry {
    fn due(&self, now: Instant) -> bool {
        self.at.is_none_or(|at| now >= at)
    }

    fn failed(&mut self, now: Instant) {
        self.at = Some(now + self.backoff.failed());
    }

    fn succeeded(&mut self) {
        self.at = None;
        self.backoff.reset();
    }
}

struct Inner {
    view: HubView,
    record: Option<HubRecord>,
    /// Set once `hub.json` has been read this process (present or absent).
    record_read: bool,
    setup: SetupState,
    last_error: Option<String>,
    setup_retry: Retry,
    key_next_poll: Option<Instant>,
    /// The hub instance has the current accounts and the account-wide streams.
    online: bool,
    online_retry: Retry,
    revocation_retry: Retry,
    /// The latest `iat` given to the hub account JWT: the server keeps the newer of two.
    account_iat: i64,
    devices: Option<Devices>,
    following_devices: bool,
    leafs_connected: Option<usize>,
    leaf_next_poll: Option<Instant>,
    tls_bucket: TokenBucket,
}

pub struct HubArea {
    deps: HubDeps,
    timing: HubTiming,
    operator_key: String,
    account_key: String,
    tls_key: String,
    inner: Mutex<Inner>,
    published: watch::Sender<HubServerState>,
    wake: Notify,
    /// One issue or removal at a time, so two never interleave their record writes.
    record_lock: tokio::sync::Mutex<()>,
    device_views: Mutex<Option<mpsc::Sender<Devices>>>,
}

fn credential_id(root: RootCredential) -> String {
    root.credential_id()
        .expect("the naming crate accepts the hub key provider tokens")
}

impl HubArea {
    pub fn new(deps: HubDeps, timing: HubTiming) -> Arc<Self> {
        let (published, _) = watch::channel(HubServerState {
            generation: unix_ms(),
            listener: None,
        });
        Arc::new(Self {
            deps,
            timing,
            operator_key: credential_id(RootCredential::BusHubOperator),
            account_key: credential_id(RootCredential::BusHubAccount),
            tls_key: credential_id(RootCredential::BusHubTls),
            inner: Mutex::new(Inner {
                view: HubView::None,
                record: None,
                record_read: false,
                setup: SetupState::NotHub,
                last_error: None,
                setup_retry: Retry::default(),
                key_next_poll: None,
                online: false,
                online_retry: Retry::default(),
                revocation_retry: Retry::default(),
                account_iat: 0,
                devices: None,
                following_devices: false,
                leafs_connected: None,
                leaf_next_poll: None,
                tls_bucket: TokenBucket::hub_tls(Instant::now()),
            }),
            published,
            wake: Notify::new(),
            record_lock: tokio::sync::Mutex::new(()),
            device_views: Mutex::new(None),
        })
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        // Every update leaves the state consistent, so a poisoned lock is still usable.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The published listener state now and after each change, for
    /// `ckbus.hub_server_watch`.
    pub fn subscribe(&self) -> watch::Receiver<HubServerState> {
        self.published.subscribe()
    }

    pub fn current(&self) -> HubServerState {
        self.published.borrow().clone()
    }

    /// The hub's `fed.` entries in health. They never change the module's status.
    pub fn health_metrics(&self) -> Map<String, Value> {
        let inner = self.inner();
        let mut metrics = Map::new();
        metrics.insert("fed.hub_setup".into(), json!(inner.setup.name()));
        metrics.insert("fed.hub_setup_error".into(), json!(inner.last_error));
        metrics.insert(
            "fed.hub_leafs_connected".into(),
            json!(inner.leafs_connected),
        );
        metrics.insert(
            "fed.hub_issued".into(),
            json!(inner
                .record
                .as_ref()
                .map_or(0, |record| record.issued.len())),
        );
        metrics
    }

    /// Starts the hub follower and the driving task. The paired-device follower starts
    /// the first time this computer is the hub.
    pub fn spawn(self: &Arc<Self>) {
        let (notices, hub_views) = mpsc::channel(16);
        tokio::spawn(follow_hub(
            self.deps.callosum.clone(),
            notices,
            self.timing.read_period,
        ));
        let (devices_tx, device_views) = mpsc::channel(16);
        *self
            .device_views
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(devices_tx);
        tokio::spawn(self.clone().run(hub_views, device_views));
    }

    async fn run(
        self: Arc<Self>,
        mut hub_views: mpsc::Receiver<HubView>,
        mut device_views: mpsc::Receiver<Devices>,
    ) {
        loop {
            self.step(Instant::now()).await;
            tokio::select! {
                view = hub_views.recv() => match view {
                    Some(view) => self.apply_hub(view),
                    None => return,
                },
                Some(devices) = device_views.recv() => self.apply_devices(devices),
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(self.timing.tick) => {}
            }
        }
    }

    /// Takes in a hub notice.
    pub fn apply_hub(&self, view: HubView) {
        let mut inner = self.inner();
        if inner.view == view {
            return;
        }
        let was_self = matches!(inner.view, HubView::SelfHub { .. });
        let is_self = matches!(view, HubView::SelfHub { .. });
        if was_self != is_self {
            log_event(
                "ckbus.federation.hub_role",
                json!({ "this_computer_is_hub": is_self }),
            );
            // Becoming the hub again re-checks the instance from the start.
            inner.online = false;
            inner.online_retry = Retry::default();
            inner.setup_retry = Retry::default();
            inner.key_next_poll = None;
        }
        inner.view = view;
    }

    /// Takes in a paired-devices snapshot.
    pub fn apply_devices(&self, devices: Devices) {
        self.inner().devices = Some(devices);
    }

    fn self_fingerprint(&self) -> Option<String> {
        match &self.inner().view {
            HubView::SelfHub { fingerprint } => Some(fingerprint.clone()),
            _ => None,
        }
    }

    /// One pass over everything due at `now`.
    pub async fn step(&self, now: Instant) {
        if self.self_fingerprint().is_none() {
            self.inner().setup = SetupState::NotHub;
            // The store under nats-hub and `hub.json` stay; only the listener goes.
            self.publish(None);
            return;
        }
        self.start_following_devices();
        if !self.ensure_record_read() {
            return;
        }
        self.ensure_setup(now).await;
        let listener = self.inner().record.as_ref().map(|r| r.cert_der.clone());
        if listener.is_none() {
            self.publish(None);
            return;
        }
        self.publish(listener);
        self.ensure_online(now).await;
        self.revoke_removed().await;
        self.push_revocations(now).await;
        self.poll_leafs(now).await;
    }

    fn publish(&self, listener: Option<Vec<u8>>) {
        self.published.send_if_modified(|state| {
            if state.listener == listener {
                return false;
            }
            // The engine ignores a state at or below the generation it holds, and it
            // outlives a ck-bus restart, so a generation is never below wall-clock
            // milliseconds: a restarted ck-bus is always ahead of its predecessor.
            state.generation = (state.generation + 1).max(unix_ms());
            state.listener = listener;
            log_event(
                "ckbus.federation.hub_listener",
                json!({
                    "generation": state.generation,
                    "listening": state.listener.is_some(),
                }),
            );
            true
        });
    }

    fn start_following_devices(&self) {
        let mut inner = self.inner();
        if inner.following_devices {
            return;
        }
        let Some(sender) = self
            .device_views
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        else {
            return;
        };
        inner.following_devices = true;
        tokio::spawn(follow_devices(
            self.deps.devices.clone(),
            sender,
            self.timing.read_period,
        ));
    }

    /// Reads `hub.json` once. A damaged record fails setup and is left as it is.
    fn ensure_record_read(&self) -> bool {
        let mut inner = self.inner();
        if inner.record_read {
            return true;
        }
        match record::read(&self.deps.files.store_root) {
            RecordFile::Absent => {}
            RecordFile::Present(record) => inner.record = Some(*record),
            RecordFile::Damaged { path, reason } => {
                let error = format!(
                    "{} is damaged ({reason}); it is left as it is",
                    path.display()
                );
                if inner.last_error.as_deref() != Some(error.as_str()) {
                    log_event(
                        "ckbus.federation.hub_setup_failed",
                        json!({ "reason": error }),
                    );
                }
                inner.setup = SetupState::Failed;
                inner.last_error = Some(error);
                return false;
            }
        }
        inner.record_read = true;
        true
    }

    /// Runs setup when there is no record, or when a hub key's `key_id` moved since the
    /// record was made; otherwise only re-reads the keys once per `key_poll`.
    async fn ensure_setup(&self, now: Instant) {
        {
            let inner = self.inner();
            let polled = inner.key_next_poll.is_some_and(|at| now < at);
            if (inner.record.is_some() && polled) || !inner.setup_retry.due(now) {
                return;
            }
        }
        let keys = self.read_keys().await;
        let (keys, previous, account_iat) = {
            let mut inner = self.inner();
            let keys = match keys {
                Ok(keys) => keys,
                Err(error) => {
                    if inner.record.is_some() {
                        // The record stays in force; the next poll tries again.
                        inner.key_next_poll = Some(now + self.timing.key_poll);
                    } else {
                        self.fail(&mut inner, now, error);
                    }
                    return;
                }
            };
            let current = inner.record.as_ref().is_some_and(|record| {
                record.operator_key_id == keys.operator.key_id
                    && record.account_key_id == keys.account.key_id
                    && record.tls_key_id == keys.tls.key_id
            });
            if current {
                inner.key_next_poll = Some(now + self.timing.key_poll);
                return;
            }
            inner.setup = SetupState::SettingUp;
            (keys, inner.record.clone(), inner.account_iat)
        };
        log_event(
            "ckbus.federation.hub_setup",
            json!({ "reason": if previous.is_some() { "key_rotated" } else { "first" } }),
        );
        let result = self.setup(previous.as_ref(), &keys, account_iat).await;
        let mut inner = self.inner();
        match result {
            Ok((record, iat)) => {
                log_event(
                    "ckbus.federation.hub_setup_written",
                    json!({
                        "operator": record.operator_public,
                        "system_account": record.system_account,
                        "hub_account": record.hub_account,
                        "spki_sha256": record.spki_sha256,
                    }),
                );
                inner.record = Some(record);
                inner.account_iat = iat;
                inner.online = false;
                inner.online_retry = Retry::default();
                inner.setup_retry.succeeded();
                inner.key_next_poll = Some(now + self.timing.key_poll);
                inner.last_error = None;
            }
            Err(error) => self.fail(&mut inner, now, error),
        }
    }

    fn fail(&self, inner: &mut Inner, now: Instant, error: String) {
        log_event(
            "ckbus.federation.hub_setup_failed",
            json!({ "reason": error }),
        );
        inner.setup = SetupState::Failed;
        inner.last_error = Some(error);
        inner.setup_retry.failed(now);
    }

    async fn read_keys(&self) -> Result<HubKeys, String> {
        let read = |id: &str| {
            let id = id.to_string();
            async move {
                self.deps
                    .vault
                    .public_key(&id)
                    .await
                    .map_err(|error| format!("{id}: {error}"))
            }
        };
        Ok(HubKeys {
            operator: read(&self.operator_key).await?,
            account: read(&self.account_key).await?,
            tls: read(&self.tls_key).await?,
        })
    }

    /// Hub setup: the operator JWT, the system account and the hub account, all signed
    /// by the hub operator key; the listener certificate; `server.conf`; then the
    /// record. Account ids, issue records and revocations carry over a rotation.
    async fn setup(
        &self,
        previous: Option<&HubRecord>,
        keys: &HubKeys,
        account_iat: i64,
    ) -> Result<(HubRecord, i64), String> {
        if let Some(port) = self.deps.local_server_url.as_deref().and_then(url_port) {
            if port == HUB_ADMIN_PORT {
                return Err(format!(
                    "the local server listens on {port}, the hub's admin port; hub setup \
                     writes nothing"
                ));
            }
        }
        let system_account = previous
            .map(|record| record.system_account.clone())
            .unwrap_or_else(new_account_identity);
        let hub_account = previous
            .map(|record| record.hub_account.clone())
            .unwrap_or_else(new_account_identity);
        let revocations = previous
            .map(|record| record.revocations.clone())
            .unwrap_or_default();
        let now = unix_secs();
        let iat = now.max(account_iat + 1);
        let vault = self.deps.vault.as_ref();
        let operator = sign_claims(vault, &self.operator_key, NkeyRole::Operator, |issuer| {
            operator_claims(issuer, &system_account, now)
        })
        .await?;
        let operator_as_account = as_account_key(&operator.signer);
        let system = sign_claims(vault, &self.operator_key, NkeyRole::Operator, |issuer| {
            system_account_claims(issuer, &system_account, &operator_as_account, iat)
        })
        .await?;
        let account_signing_key = as_account_key(&keys.account);
        let hub = sign_claims(vault, &self.operator_key, NkeyRole::Operator, |issuer| {
            hub_account_claims(
                issuer,
                &hub_account,
                &account_signing_key,
                &revocations,
                iat,
            )
        })
        .await?;
        let operator_public =
            crate::credentials::nkey::encode_public(NkeyRole::Operator, &operator.signer.public);
        // Reissued only when the TLS key rotates, so the SPKI callosum announces and the
        // bytes the engine serves stay the same across restarts.
        let cert_der = match previous {
            Some(record)
                if record.tls_key_id == keys.tls.key_id
                    && certificate_public_key(&record.cert_der) == Some(keys.tls.public) =>
            {
                record.cert_der.clone()
            }
            _ => self_issued_certificate(&keys.tls.public),
        };
        let files = &self.deps.files;
        for dir in [&files.hub_dir, &files.jwt_dir(), &files.js_dir()] {
            fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        }
        let conf = HubConf {
            js_dir: &files.js_dir(),
            operator_jwt: &files.operator_jwt(),
            jwt_dir: &files.jwt_dir(),
            system_account: &system_account,
            system_account_jwt: &system.jwt,
            hub_account: &hub_account,
            hub_account_jwt: &hub.jwt,
        }
        .render()?;
        write_text(&files.operator_jwt(), &operator.jwt)?;
        write_text(&files.server_conf(), &conf)?;
        let record = HubRecord {
            operator_public,
            operator_key_id: operator.signer.key_id.clone(),
            account_key_id: keys.account.key_id.clone(),
            tls_key_id: keys.tls.key_id.clone(),
            system_account,
            hub_account,
            spki_sha256: spki_sha256_hex(&keys.tls.public),
            cert_der,
            issued: previous.map(|r| r.issued.clone()).unwrap_or_default(),
            removed: previous.map(|r| r.removed.clone()).unwrap_or_default(),
            revocations,
            // The preloaded hub account JWT carries every revocation, and the instance
            // gets the same JWT pushed when it comes up.
            revocations_pushed: false,
        };
        record::write(&files.store_root, &record)?;
        Ok((record, iat))
    }

    /// Once per hub role and after each setup: pushes the hub account JWT through the
    /// system account and creates the account-wide streams and every issued box's
    /// inbox. Until the instance answers, setup stays `setting_up` and is retried.
    async fn ensure_online(&self, now: Instant) {
        let record = {
            let inner = self.inner();
            if inner.online || !inner.online_retry.due(now) {
                return;
            }
            match inner.record.clone() {
                Some(record) => record,
                None => return,
            }
        };
        let result = self.bring_online(&record).await;
        let mut inner = self.inner();
        match result {
            Ok(iat) => {
                log_event(
                    "ckbus.federation.hub_ready",
                    json!({ "hub_account": record.hub_account }),
                );
                inner.online = true;
                inner.online_retry.succeeded();
                inner.setup = SetupState::Ready;
                inner.last_error = None;
                inner.account_iat = iat;
                if let Some(current) = inner.record.as_mut() {
                    if current.revocations == record.revocations {
                        current.revocations_pushed = true;
                    }
                }
            }
            Err(error) => {
                if inner.last_error.as_deref() != Some(error.as_str()) {
                    log_event(
                        "ckbus.federation.hub_instance_unavailable",
                        json!({ "reason": error }),
                    );
                }
                inner.setup = SetupState::SettingUp;
                inner.last_error = Some(error);
                inner.online_retry.failed(now);
            }
        }
    }

    async fn bring_online(&self, record: &HubRecord) -> Result<i64, String> {
        let iat = self.push_hub_account(record).await?;
        let mut streams = vec![
            HubStream::IdLog,
            HubStream::Residence,
            HubStream::Cuts,
            HubStream::Rooms,
            HubStream::Memory,
        ];
        let mut boxes: Vec<&str> = record.issued.values().map(|i| i.box_id.as_str()).collect();
        boxes.sort_unstable();
        boxes.dedup();
        streams.extend(boxes.into_iter().map(|box_id| HubStream::Inbox {
            box_id: box_id.to_string(),
        }));
        self.deps.server.ensure_streams(record, &streams).await?;
        Ok(iat)
    }

    /// Re-signs the hub account JWT with the record's revocations and pushes it.
    async fn push_hub_account(&self, record: &HubRecord) -> Result<i64, String> {
        let account = self
            .deps
            .vault
            .public_key(&self.account_key)
            .await
            .map_err(|error| format!("{}: {error}", self.account_key))?;
        let account_signing_key = as_account_key(&account);
        let iat = unix_secs().max(self.inner().account_iat + 1);
        let signed = sign_claims(
            self.deps.vault.as_ref(),
            &self.operator_key,
            NkeyRole::Operator,
            |issuer| {
                hub_account_claims(
                    issuer,
                    &record.hub_account,
                    &account_signing_key,
                    &record.revocations,
                    iat,
                )
            },
        )
        .await?;
        self.inner().account_iat = iat;
        self.deps
            .server
            .apply_account(record, &record.hub_account, &signed.jwt)
            .await?;
        Ok(iat)
    }

    /// Moves every issued computer that the paired devices no longer list as live to
    /// the removed list and revokes its leaf key in the record. The push follows.
    async fn revoke_removed(&self) {
        let _guard = self.record_lock.lock().await;
        let self_fp = self.self_fingerprint();
        let mut inner = self.inner();
        let Some(devices) = inner.devices.clone() else {
            return;
        };
        let Some(record) = inner.record.as_mut() else {
            return;
        };
        let gone: Vec<String> = record
            .issued
            .keys()
            .filter(|fp| Some(fp.as_str()) != self_fp.as_deref() && devices.is_removed(fp))
            .cloned()
            .collect();
        if gone.is_empty() {
            return;
        }
        let now = unix_secs();
        for fingerprint in gone {
            let Some(issued) = record.issued.remove(&fingerprint) else {
                continue;
            };
            log_event(
                "ckbus.federation.hub_leaf_revoked",
                json!({
                    "fingerprint": fingerprint,
                    "box": issued.box_id,
                    "leaf_nkey": issued.leaf_nkey,
                }),
            );
            record.revocations.insert(issued.leaf_nkey.clone(), now);
            record.removed.insert(
                fingerprint,
                Removed {
                    box_id: issued.box_id,
                    leaf_nkey: issued.leaf_nkey,
                    removed_at: now,
                },
            );
        }
        record.revocations_pushed = false;
        let snapshot = record.clone();
        inner.revocation_retry = Retry::default();
        drop(inner);
        if let Err(error) = record::write(&self.deps.files.store_root, &snapshot) {
            log_event(
                "ckbus.federation.hub_record_unwritten",
                json!({ "reason": error }),
            );
        }
    }

    /// Pushes recorded revocations the instance has not read back yet. The claims
    /// update itself closes the revoked leaf's connection.
    async fn push_revocations(&self, now: Instant) {
        let record = {
            let inner = self.inner();
            if !inner.online || !inner.revocation_retry.due(now) {
                return;
            }
            match inner.record.as_ref() {
                Some(record) if !record.revocations_pushed => record.clone(),
                _ => return,
            }
        };
        let result = self.push_hub_account(&record).await;
        let mut inner = self.inner();
        match result {
            Ok(_) => {
                log_event(
                    "ckbus.federation.hub_revocations_pushed",
                    json!({ "revoked": record.revocations.len() }),
                );
                inner.revocation_retry.succeeded();
                // Leaf counts are read again at once, so health shows the kick.
                inner.leaf_next_poll = None;
                if let Some(current) = inner.record.as_mut() {
                    if current.revocations == record.revocations {
                        current.revocations_pushed = true;
                        let snapshot = current.clone();
                        drop(inner);
                        let _ = record::write(&self.deps.files.store_root, &snapshot);
                    }
                }
            }
            Err(error) => {
                log_event(
                    "ckbus.federation.hub_revocations_unpushed",
                    json!({ "reason": error }),
                );
                inner.revocation_retry.failed(now);
            }
        }
    }

    async fn poll_leafs(&self, now: Instant) {
        let record = {
            let inner = self.inner();
            if !inner.online || inner.leaf_next_poll.is_some_and(|at| now < at) {
                return;
            }
            inner.record.clone()
        };
        let Some(record) = record else { return };
        let count = self.deps.server.leaf_count(&record).await.ok();
        let mut inner = self.inner();
        inner.leafs_connected = count;
        inner.leaf_next_poll = Some(now + self.timing.leaf_poll);
    }

    /// `ckbus.hub_identity_read`: `{spki_sha256, leaf_port}` while this computer is the
    /// hub and setup has written its certificate, `{hub: null}` otherwise.
    pub fn identity_json(&self) -> Value {
        match self.identity() {
            Some(identity) => json!({
                "spki_sha256": identity.spki_sha256,
                "leaf_port": identity.leaf_port,
            }),
            None => json!({ "hub": null }),
        }
    }

    fn identity(&self) -> Option<LocalHubIdentity> {
        let inner = self.inner();
        if !matches!(inner.view, HubView::SelfHub { .. }) {
            return None;
        }
        inner.record.as_ref().map(|record| LocalHubIdentity {
            spki_sha256: record.spki_sha256.clone(),
            leaf_port: HUB_LEAF_PORT,
        })
    }

    /// `ckbus.hub_tls_sign`: one TLS 1.3 handshake signature for the hub listener.
    pub async fn tls_sign(&self, message_b64: &str, scheme: &str) -> Result<Value, Refusal> {
        let taken = self.inner().tls_bucket.try_take(Instant::now());
        if !taken {
            let wait = self.inner().tls_bucket.next_token_in();
            return Err(Refusal::new(
                tls_code::RATE_LIMITED,
                format!("more than 20 hub handshakes a second; next in {wait:?}"),
            ));
        }
        if scheme != "ed25519" {
            return Err(Refusal::new(
                tls_code::BAD_REQUEST,
                format!("scheme {scheme:?} is not ed25519"),
            ));
        }
        let message = URL_SAFE_NO_PAD
            .decode(message_b64)
            .map_err(|_| Refusal::new(tls_code::BAD_REQUEST, "message_b64 is not base64url"))?;
        if !is_certificate_verify_content(&message) {
            return Err(Refusal::new(
                tls_code::INVALID_MESSAGE,
                "the message is not TLS 1.3 server CertificateVerify content",
            ));
        }
        let tls_key_id = {
            let inner = self.inner();
            match (&inner.view, &inner.record) {
                (HubView::SelfHub { .. }, Some(record)) => record.tls_key_id.clone(),
                _ => {
                    return Err(Refusal::new(
                        tls_code::NOT_HUB,
                        "this computer serves no hub listener",
                    ))
                }
            }
        };
        let signed = tokio::time::timeout(
            self.timing.sign_timeout,
            self.deps.vault.sign(&self.tls_key, &message),
        )
        .await;
        let signature = match signed {
            Ok(Ok(signature)) => signature,
            Ok(Err(error)) => {
                return Err(Refusal::new(tls_code::VAULT_UNAVAILABLE, error.to_string()))
            }
            Err(_) => {
                return Err(Refusal::new(
                    tls_code::VAULT_UNAVAILABLE,
                    format!(
                        "credential.sign took longer than {:?}",
                        self.timing.sign_timeout
                    ),
                ))
            }
        };
        if signature.key_id != tls_key_id {
            // The certificate names the old key, so a client would refuse this
            // signature. Setup runs again now and reissues the certificate.
            self.inner().key_next_poll = None;
            self.wake.notify_one();
            return Err(Refusal::new(
                tls_code::KEY_ROTATED,
                "the hub TLS key changed since its certificate was issued",
            ));
        }
        Ok(json!({ "signature_b64": URL_SAFE_NO_PAD.encode(signature.signature) }))
    }

    /// `ckbus.hub_leaf_issue` for the paired computer `fingerprint`.
    pub async fn leaf_issue(&self, fingerprint: &str, leaf_nkey: &str) -> Result<String, Refusal> {
        self.ready_to_issue()?;
        check_leaf_nkey(leaf_nkey)?;
        let devices = self
            .deps
            .devices
            .read()
            .await
            .map_err(|error| Refusal::new(issue_code::CALLOSUM_UNAVAILABLE, error))?;
        let devices = parse_devices(&devices)
            .map_err(|error| Refusal::new(issue_code::CALLOSUM_UNAVAILABLE, error))?;
        if !devices.is_live_computer(fingerprint) {
            return Err(Refusal::new(
                issue_code::NOT_PAIRED,
                format!("{fingerprint} is not a live paired computer"),
            ));
        }
        let peers = self
            .deps
            .devices
            .peer_keys()
            .await
            .map_err(|error| Refusal::new(issue_code::CALLOSUM_UNAVAILABLE, error))?;
        let box_id = machine_id_for(&peers, fingerprint)
            .map_err(|error| Refusal::new(issue_code::CALLOSUM_UNAVAILABLE, error))?
            .ok_or_else(|| {
                Refusal::new(
                    issue_code::NO_MACHINE_ID,
                    format!("callosum binds no machine id to {fingerprint}"),
                )
            })?;
        self.issue_for(fingerprint, &box_id, leaf_nkey).await
    }

    fn ready_to_issue(&self) -> Result<(), Refusal> {
        let inner = self.inner();
        if !matches!(inner.view, HubView::SelfHub { .. }) {
            return Err(Refusal::new(
                issue_code::NOT_HUB,
                "this computer is not the hub",
            ));
        }
        if !inner.online {
            return Err(Refusal::new(
                issue_code::HUB_NOT_READY,
                format!(
                    "hub setup is {}{}",
                    inner.setup.name(),
                    inner
                        .last_error
                        .as_ref()
                        .map(|error| format!(": {error}"))
                        .unwrap_or_default()
                ),
            ));
        }
        Ok(())
    }

    /// Signs a 24 h leaf user JWT for `leaf_nkey` carrying `{box}`'s grant, creates the
    /// box's hub inbox if missing, and records the issue under `fingerprint`.
    async fn issue_for(
        &self,
        fingerprint: &str,
        box_id: &str,
        leaf_nkey: &str,
    ) -> Result<String, Refusal> {
        let (publish, subscribe) = leaf_permissions(box_id)
            .map_err(|error| Refusal::new(issue_code::INVALID_MACHINE_ID, error))?;
        let _guard = self.record_lock.lock().await;
        let record = self
            .inner()
            .record
            .clone()
            .ok_or_else(|| Refusal::new(issue_code::HUB_NOT_READY, "hub setup has not run"))?;
        // A key revoked at second `t` refuses every JWT issued at or before `t`, so a
        // re-paired computer's new JWT must be issued after its old cutoff.
        let now = unix_secs();
        let issued_at = record
            .revocations
            .get(leaf_nkey)
            .map_or(now, |cutoff| now.max(cutoff + 1));
        let name = format!("leaf_{box_id}");
        let user = UserJwt {
            user_public: leaf_nkey,
            issuer_account: &record.hub_account,
            name: &name,
            issued_at,
            expires_at: issued_at + LEAF_JWT_LIFETIME_SECS,
            publish: &publish,
            subscribe: &subscribe,
        };
        let signed = sign_claims(
            self.deps.vault.as_ref(),
            &self.account_key,
            NkeyRole::Account,
            |issuer| user.claims(issuer),
        )
        .await
        .map_err(|error| Refusal::new(issue_code::VAULT_UNAVAILABLE, error))?;
        if signed.signer.key_id != record.account_key_id {
            // The hub account lists the old signing key, so the server would refuse this
            // JWT. Setup runs again now and re-signs the account.
            self.inner().key_next_poll = None;
            self.wake.notify_one();
            return Err(Refusal::new(
                issue_code::HUB_NOT_READY,
                "the hub account key changed since the hub account was signed",
            ));
        }
        let first_for_box = !record.issued.values().any(|i| i.box_id == box_id);
        if first_for_box || !record.issued.contains_key(fingerprint) {
            self.deps
                .server
                .ensure_streams(
                    &record,
                    &[HubStream::Inbox {
                        box_id: box_id.to_string(),
                    }],
                )
                .await
                .map_err(|error| Refusal::new(issue_code::HUB_UNAVAILABLE, error))?;
        }
        let entry = Issued {
            box_id: box_id.to_string(),
            leaf_nkey: leaf_nkey.to_string(),
            issued_at,
        };
        let snapshot = {
            let mut inner = self.inner();
            let Some(current) = inner.record.as_mut() else {
                return Err(Refusal::new(
                    issue_code::HUB_NOT_READY,
                    "hub setup has not run",
                ));
            };
            let changed = current.issued.get(fingerprint) != Some(&entry);
            current.removed.remove(fingerprint);
            current.issued.insert(fingerprint.to_string(), entry);
            changed.then(|| current.clone())
        };
        if let Some(snapshot) = snapshot {
            record::write(&self.deps.files.store_root, &snapshot)
                .map_err(|error| Refusal::new(issue_code::HUB_UNAVAILABLE, error))?;
        }
        log_event(
            "ckbus.federation.hub_leaf_issued",
            json!({
                "fingerprint": fingerprint,
                "box": box_id,
                "leaf_nkey": leaf_nkey,
                "expires_at": issued_at + LEAF_JWT_LIFETIME_SECS,
            }),
        );
        Ok(signed.jwt)
    }
}

/// The three hub keys' public halves as the vault reports them.
struct HubKeys {
    operator: VaultPublicKey,
    account: VaultPublicKey,
    tls: VaultPublicKey,
}

/// The hub machine's own leaf credential and listener, for its own leaf area.
#[async_trait]
impl LocalHub for HubArea {
    async fn identity(&self) -> Result<LocalHubIdentity, String> {
        HubArea::identity(self).ok_or_else(|| "hub setup has not written a certificate".into())
    }

    async fn issue(&self, leaf_nkey: &str) -> Result<String, String> {
        let describe = |refusal: Refusal| format!("{}: {}", refusal.code, refusal.message);
        self.ready_to_issue().map_err(describe)?;
        check_leaf_nkey(leaf_nkey).map_err(describe)?;
        let fingerprint = self
            .self_fingerprint()
            .ok_or("this computer is not the hub")?;
        let box_id = self.deps.own.machine_id().await?;
        self.issue_for(&fingerprint, &box_id, leaf_nkey)
            .await
            .map_err(describe)
    }
}

fn check_leaf_nkey(leaf_nkey: &str) -> Result<(), Refusal> {
    if leaf_nkey.starts_with('U') && KeyPair::from_public_key(leaf_nkey).is_ok() {
        return Ok(());
    }
    Err(Refusal::new(
        issue_code::BAD_REQUEST,
        "leaf_nkey must be a user public nkey",
    ))
}

/// TLS 1.3 server `CertificateVerify` content (RFC 8446 section 4.4.3): 64 spaces, the
/// context string, a zero byte, then a SHA-256 or SHA-384 transcript hash. The vault
/// refuses anything else under the TLS key; checking here first keeps a malformed
/// request from spending a vault call.
pub fn is_certificate_verify_content(message: &[u8]) -> bool {
    const CONTEXT: &[u8] = b"TLS 1.3, server CertificateVerify\0";
    let prefix = 64 + CONTEXT.len();
    message.len() > prefix
        && message[..64].iter().all(|byte| *byte == b' ')
        && &message[64..prefix] == CONTEXT
        && matches!(message.len() - prefix, 32 | 48)
}

/// Follows `callosum.paired_devices_watch`, and reads `callosum.paired_devices_read`
/// every `period` while the watch is down. A snapshot that does not parse is dropped, so
/// it is never read as every computer leaving.
async fn follow_devices(
    devices: Arc<dyn PairedDevices>,
    views: mpsc::Sender<Devices>,
    period: Duration,
) {
    let mut retry = Duration::from_secs(1).min(period);
    let mut next_read = Instant::now();
    let forward = |body: Vec<u8>| match parse_devices(&body) {
        Ok(snapshot) => Some(snapshot),
        Err(reason) => {
            log_event(
                "ckbus.federation.paired_devices_unreadable",
                json!({ "reason": reason }),
            );
            None
        }
    };
    loop {
        match devices.watch().await {
            Ok(mut notices) => {
                retry = Duration::from_secs(1).min(period);
                while let Some(body) = notices.recv().await {
                    if let Some(snapshot) = forward(body) {
                        if views.send(snapshot).await.is_err() {
                            return;
                        }
                    }
                }
                next_read = Instant::now();
            }
            Err(reason) => log_event(
                "ckbus.federation.paired_devices_watch",
                json!({ "state": "unavailable", "reason": reason }),
            ),
        }
        if Instant::now() >= next_read {
            if let Ok(body) = devices.read().await {
                if let Some(snapshot) = forward(body) {
                    if views.send(snapshot).await.is_err() {
                        return;
                    }
                }
            }
            next_read = Instant::now() + period;
        }
        tokio::time::sleep(retry.min(next_read.saturating_duration_since(Instant::now()))).await;
        retry = (retry * 2).min(period);
    }
}

/// Writes a text file through a sibling `*.tmp`, so a reader never sees half of it.
fn write_text(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text)
        .and_then(|()| cortexkit_lease::durable_replace(&tmp, path))
        .map_err(|error: io::Error| format!("{}: {error}", path.display()))
}

/// For tests: the issue records as `{fingerprint: (box, nkey)}`.
#[cfg(test)]
pub fn issued(area: &HubArea) -> BTreeMap<String, (String, String)> {
    area.inner()
        .record
        .as_ref()
        .map(|record| {
            record
                .issued
                .iter()
                .map(|(fp, i)| (fp.clone(), (i.box_id.clone(), i.leaf_nkey.clone())))
                .collect()
        })
        .unwrap_or_default()
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

fn unix_secs() -> i64 {
    (unix_ms() / 1000) as i64
}
