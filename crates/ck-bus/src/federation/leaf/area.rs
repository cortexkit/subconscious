//! The leaf area's state and the task that drives it.
//!
//! One task owns every decision: it follows callosum's hub row, makes sure the
//! federation account exists, reads the leaf key, gets and renews the hub's JWT for it,
//! builds the remote, watches whether the link is up (to fall back to callosum's relay
//! bridge, and to bounce the link after a renewal), and publishes `{generation,
//! remotes}` to every `ckbus.leaf_watch`. `ckbus.leaf_sign` only reads the published
//! remote and the current JWT, so a signature never waits on that task.

use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Map, Value};
use tokio::{
    sync::{mpsc, watch, Notify},
    time::Instant,
};

use super::{
    credential::{Backoff, LeafCredential, LeafKey},
    hub::{parse_hub_body, HubView},
    remote::Remote,
    seams::{
        Callosum, FederationAccounts, LeafLinks, LocalHub, LocalHubIdentity, OutboundQuiet,
        PeerIssuer,
    },
    sign_code,
};
use crate::{
    bootstrap::plane::LeafLink,
    credentials::{roots::RootCredential, vault::VaultSigning},
};

/// How often things happen. Production uses `Timing::default()`; tests shorten it.
#[derive(Debug, Clone)]
pub struct Timing {
    /// `callosum.hub_read` cadence while the watch is down, and the longest wait
    /// between attempts to reopen the watch.
    pub hub_read_period: Duration,
    /// How often the leaf key's public half is re-read, to notice an operator rotation.
    pub key_poll: Duration,
    /// How often the local server's leaf report is read.
    pub link_poll: Duration,
    /// How long one candidate address may take to connect before the round counts it
    /// failed: a TLS handshake, the engine's 5 s wait for this module's signature, and the
    /// server's 1 s reconnect delay.
    pub round_per_url: Duration,
    /// How long the outbound side must be idle before a renewal kick.
    pub quiet_window: Duration,
    /// The latest a renewal kick waits for a quiet moment.
    pub kick_cap: Duration,
    /// The driving task's idle wake-up.
    pub tick: Duration,
    /// The bound on one `credential.sign`, inside the wrapper's 4 s bound on
    /// `ckbus.leaf_sign`, so the wrapper always gets this module's reason.
    pub sign_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            hub_read_period: Duration::from_secs(60),
            key_poll: Duration::from_secs(60),
            link_poll: Duration::from_secs(5),
            round_per_url: Duration::from_secs(10),
            quiet_window: Duration::from_secs(2),
            kick_cap: Duration::from_secs(3600),
            tick: Duration::from_secs(1),
            sign_timeout: Duration::from_millis(3_500),
        }
    }
}

/// Everything outside the area.
pub struct LeafDeps {
    pub callosum: Arc<dyn Callosum>,
    pub issuer: Arc<dyn PeerIssuer>,
    pub local_hub: Arc<dyn LocalHub>,
    pub vault: Arc<dyn VaultSigning>,
    pub links: Arc<dyn LeafLinks>,
    pub quiet: Arc<dyn OutboundQuiet>,
    pub accounts: Arc<dyn FederationAccounts>,
}

/// What `ckbus.leaf_watch` sends: the engine's `state` without its `v` and `type`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafState {
    pub generation: u64,
    pub remotes: Vec<Remote>,
    decided: bool,
}

impl LeafState {
    pub fn watch_json(&self) -> Option<Value> {
        self.decided.then(|| self.to_json())
    }

    pub fn to_json(&self) -> Value {
        json!({
            "generation": self.generation,
            "remotes": self.remotes.iter().map(Remote::to_json).collect::<Vec<_>>(),
        })
    }
}

/// Why `ckbus.leaf_sign` produced no signature. The code is the short reason the
/// wrapper passes to the engine, which logs "leaf signer via ck-bus: <code>".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignRefusal {
    pub code: &'static str,
    pub message: String,
}

impl SignRefusal {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// A renewal or key change replaced the JWT the live connection presented; the
/// connection is bounced once so its reconnect signs with the new one.
#[derive(Debug, Clone, Copy)]
struct KickPlan {
    deadline: Instant,
}

/// What the area knows about the link from the local server's leaf report.
#[derive(Debug, Default)]
struct LinkTrack {
    next_poll: Option<Instant>,
    /// The last report: `None` before the first one or after a read that failed.
    connected: Option<bool>,
    links: Vec<LeafLink>,
    down_since: Option<Instant>,
    bridge_opened_at: Option<Instant>,
    read_failure_logged: bool,
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
    hub: HubView,
    hub_known: bool,
    account: Option<String>,
    account_retry: Retry,
    local: Option<LocalHubIdentity>,
    local_retry: Option<Instant>,
    local_failure: Option<String>,
    key: Option<LeafKey>,
    key_next_poll: Option<Instant>,
    key_backoff: Backoff,
    credential: Option<LeafCredential>,
    renew_due: Option<Instant>,
    issue_retry: Retry,
    /// Set when the current JWT was dropped because the leaf key changed, so the one that
    /// replaces it also bounces the connection.
    replacing: bool,
    bridge: Option<u16>,
    link: LinkTrack,
    kick: Option<KickPlan>,
    remote_state: &'static str,
}

impl Inner {
    fn new() -> Self {
        Self {
            hub: HubView::None,
            hub_known: false,
            account: None,
            account_retry: Retry::default(),
            local: None,
            local_retry: None,
            local_failure: None,
            key: None,
            key_next_poll: None,
            key_backoff: Backoff::default(),
            credential: None,
            renew_due: None,
            issue_retry: Retry::default(),
            replacing: false,
            bridge: None,
            link: LinkTrack::default(),
            kick: None,
            remote_state: "none",
        }
    }

    /// Forgets everything tied to the previous hub computer.
    fn leave_hub(&mut self) {
        self.local = None;
        self.local_retry = None;
        self.local_failure = None;
        self.credential = None;
        self.renew_due = None;
        self.issue_retry = Retry::default();
        self.replacing = false;
        self.bridge = None;
        self.link = LinkTrack::default();
        self.kick = None;
    }
}

pub struct LeafArea {
    deps: LeafDeps,
    timing: Timing,
    leaf_key_id: String,
    inner: Mutex<Inner>,
    published: watch::Sender<LeafState>,
    /// Wakes the driving task early, for example when a signature shows the key moved.
    wake: Notify,
    wall_clock: Box<dyn Fn() -> i64 + Send + Sync>,
}

impl LeafArea {
    pub fn new(deps: LeafDeps, timing: Timing) -> Arc<Self> {
        Self::with_clock(deps, timing, unix_secs)
    }

    pub(crate) fn with_clock(
        deps: LeafDeps,
        timing: Timing,
        wall_clock: impl Fn() -> i64 + Send + Sync + 'static,
    ) -> Arc<Self> {
        let leaf_key_id = RootCredential::BusLeaf
            .credential_id()
            .expect("the naming crate accepts the bus-leaf provider token");
        let (published, _) = watch::channel(LeafState {
            generation: unix_ms(),
            remotes: Vec::new(),
            decided: false,
        });
        Arc::new(Self {
            deps,
            timing,
            leaf_key_id,
            inner: Mutex::new(Inner::new()),
            published,
            wake: Notify::new(),
            wall_clock: Box::new(wall_clock),
        })
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        // Every update leaves the state consistent, so a poisoned lock is still usable.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The published state now and after each change, for `ckbus.leaf_watch`.
    pub fn subscribe(&self) -> watch::Receiver<LeafState> {
        self.published.subscribe()
    }

    pub fn current(&self) -> LeafState {
        self.published.borrow().clone()
    }

    pub fn has_hub_link(&self) -> bool {
        let inner = self.inner();
        inner.link.connected == Some(true)
            && matches!(inner.remote_state, "connected" | "connected_via_bridge")
    }

    /// The `fed.` entries health carries. They never change the module's status: the
    /// local bus works without a hub.
    pub fn health_metrics(&self) -> Map<String, Value> {
        let inner = self.inner();
        let mut metrics = Map::new();
        metrics.insert("fed.hub_kind".into(), json!(inner.hub.kind()));
        metrics.insert("fed.remote_state".into(), json!(inner.remote_state));
        metrics.insert(
            "fed.jwt_expires_at_ms".into(),
            json!(inner
                .credential
                .as_ref()
                .map(|credential| credential.expires_at.saturating_mul(1000))),
        );
        metrics
    }

    /// Starts the hub follower and the driving task.
    pub fn spawn(self: &Arc<Self>) {
        let (notices, hub_views) = mpsc::channel(16);
        tokio::spawn(follow_hub(
            self.deps.callosum.clone(),
            notices,
            self.timing.hub_read_period,
        ));
        tokio::spawn(self.clone().run(hub_views));
    }

    async fn run(self: Arc<Self>, mut hub_views: mpsc::Receiver<HubView>) {
        loop {
            self.step(Instant::now()).await;
            let now = Instant::now();
            let mut sleep = self
                .timing
                .tick
                .min(crate::credentials::renewal::WALL_CLOCK_RECHECK);
            if let Some(hint) = self.inner().renew_due.filter(|hint| *hint > now) {
                sleep = sleep.min(hint - now);
            }
            tokio::select! {
                view = hub_views.recv() => match view {
                    Some(view) => self.apply_hub(view),
                    None => return,
                },
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(sleep) => {}
            }
        }
    }

    /// Takes in a hub notice. A different hub computer drops the credential, bridge and
    /// link state of the old one; a changed address list or pin for the same hub keeps
    /// the credential.
    pub fn apply_hub(&self, view: HubView) {
        let mut inner = self.inner();
        inner.hub_known = true;
        if inner.hub == view {
            return;
        }
        if inner.hub.kind() != view.kind() || inner.hub.fingerprint() != view.fingerprint() {
            inner.leave_hub();
        }
        log_event(
            "ckbus.federation.hub",
            match &view {
                HubView::Refused { reason } => json!({ "kind": "refused", "reason": reason }),
                HubView::Hosted => json!({
                    "kind": "hosted",
                    "residual": "this build does not dial the hosted hub; no remote",
                }),
                other => json!({ "kind": other.kind(), "fingerprint": other.fingerprint() }),
            },
        );
        inner.hub = view;
    }

    /// One pass over everything due at `now`.
    pub async fn step(&self, now: Instant) {
        let hub = {
            let inner = self.inner();
            if !inner.hub_known {
                return;
            }
            inner.hub.clone()
        };
        let state = match hub {
            HubView::None => "none",
            HubView::Hosted => "hosted_unsupported",
            HubView::Refused { .. } => "refused",
            _ => "",
        };
        if !state.is_empty() {
            return self.settle(state, true);
        }
        let Some(account) = self.ensure_account(now).await else {
            return self.settle("waiting_account", false);
        };
        let local = match &hub {
            HubView::SelfHub { .. } => match self.ensure_local(now).await {
                Some(local) => Some(local),
                None => return self.settle("self_hub_unavailable", false),
            },
            _ => None,
        };
        let Some(key) = self.ensure_key(now).await else {
            return self.settle("waiting_leaf_key", false);
        };
        if !self.ensure_credential(now, &hub, &key).await {
            return self.settle("waiting_credential", false);
        }
        // The leaf report first: a bridge it opens joins this pass's remote.
        self.watch_link(now, &hub, &account).await;
        let bridge = self.inner().bridge;
        let remote = match (&hub, &local) {
            (
                HubView::Peer {
                    spki_sha256,
                    candidates,
                    ..
                },
                _,
            ) => {
                let mut urls: Vec<String> = candidates.iter().map(|c| c.url()).collect();
                urls.extend(bridge.map(|port| format!("tls://127.0.0.1:{port}")));
                (!urls.is_empty()).then(|| Remote::spki(&account, urls, spki_sha256))
            }
            (HubView::SelfHub { .. }, Some(local)) => Some(Remote::spki(
                &account,
                vec![format!("tls://127.0.0.1:{}", local.leaf_port)],
                &local.spki_sha256,
            )),
            _ => None,
        };
        self.publish(remote.iter().cloned().collect());
        let mut inner = self.inner();
        // The bridge is only the last URL the engine tries, so an open bridge says nothing
        // about the live link. Report it only when a link's peer is the bridge's own port.
        let via_bridge = inner.bridge.is_some_and(|port| {
            inner
                .link
                .links
                .iter()
                .any(|link| link.ip == "127.0.0.1" && link.port == port)
        });
        inner.remote_state = match (remote.is_some(), inner.link.connected) {
            (false, _) => "no_address",
            (true, Some(true)) if via_bridge => "connected_via_bridge",
            (true, Some(true)) => "connected",
            (true, _) => "connecting",
        };
    }

    /// Once the first decision has been published, a later loss of credentials still
    /// withdraws the old remote. Before that, leave the engine's running link alone.
    fn settle(&self, remote_state: &'static str, complete: bool) {
        self.inner().remote_state = remote_state;
        if complete || self.published.borrow().decided {
            self.publish(Vec::new());
        }
    }

    fn publish(&self, remotes: Vec<Remote>) {
        self.published.send_if_modified(|state| {
            if state.decided && state.remotes == remotes {
                return false;
            }
            // The engine ignores a state at or below the generation it holds, and it
            // outlives a ck-bus restart, so a generation is never below wall-clock
            // milliseconds: a restarted ck-bus is always ahead of its predecessor.
            state.generation = (state.generation + 1).max(unix_ms());
            state.remotes = remotes;
            state.decided = true;
            log_event(
                "ckbus.federation.leaf_state",
                json!({
                    "generation": state.generation,
                    "remotes": state.remotes.iter().map(|r| json!({
                        "name": r.name,
                        "urls": r.urls,
                    })).collect::<Vec<_>>(),
                }),
            );
            true
        });
    }

    async fn ensure_account(&self, now: Instant) -> Option<String> {
        {
            let inner = self.inner();
            if inner.account.is_some() || !inner.account_retry.due(now) {
                return inner.account.clone();
            }
        }
        let result = self.deps.accounts.ensure().await;
        let mut inner = self.inner();
        match result {
            Ok(account) => {
                log_event(
                    "ckbus.federation.account_ready",
                    json!({ "account_public": account }),
                );
                inner.account_retry.succeeded();
                inner.account = Some(account);
            }
            Err(reason) => {
                log_event(
                    "ckbus.federation.account_unavailable",
                    json!({ "reason": reason }),
                );
                inner.account_retry.failed(now);
            }
        }
        inner.account.clone()
    }

    async fn ensure_local(&self, now: Instant) -> Option<LocalHubIdentity> {
        {
            let inner = self.inner();
            if inner.local_retry.is_some_and(|at| now < at) {
                return inner.local.clone();
            }
        }
        // Re-read once per `key_poll`, so a hub certificate reissued for a rotated TLS
        // key reaches the pin; a failed re-read keeps the identity already known.
        let result = self.deps.local_hub.identity().await;
        let mut inner = self.inner();
        inner.local_retry = Some(now + self.timing.key_poll);
        match result {
            Ok(local) => {
                inner.local = Some(local);
                inner.local_failure = None;
            }
            Err(reason) => {
                // Logged once per distinct reason, then retried quietly.
                if inner.local_failure.as_deref() != Some(reason.as_str()) {
                    log_event(
                        "ckbus.federation.self_hub_unavailable",
                        json!({ "reason": reason, "residual": "no remote until it answers" }),
                    );
                    inner.local_failure = Some(reason);
                }
            }
        }
        inner.local.clone()
    }

    /// Reads the leaf key's public half once per `key_poll`. A new `key_id` means the
    /// operator replaced the key: the JWT naming the old one is dropped and a new one
    /// requested at once.
    async fn ensure_key(&self, now: Instant) -> Option<LeafKey> {
        {
            let inner = self.inner();
            if inner.key_next_poll.is_some_and(|at| now < at) {
                return inner.key.clone();
            }
        }
        let result = self.deps.vault.public_key(&self.leaf_key_id).await;
        let mut inner = self.inner();
        match result {
            Ok(public) => {
                let key = LeafKey::from_vault(&public);
                let stale = inner
                    .credential
                    .as_ref()
                    .is_some_and(|credential| credential.key_id != key.key_id);
                if stale {
                    log_event(
                        "ckbus.federation.leaf_key_changed",
                        json!({ "key_id": key.key_id, "leaf_nkey": key.nkey }),
                    );
                    inner.credential = None;
                    inner.renew_due = None;
                    inner.issue_retry = Retry::default();
                    inner.replacing = true;
                }
                inner.key = Some(key);
                inner.key_backoff.reset();
                inner.key_next_poll = Some(now + self.timing.key_poll);
            }
            Err(error) => {
                log_event(
                    "ckbus.federation.leaf_key_unavailable",
                    json!({ "credential_id": self.leaf_key_id, "reason": error.to_string() }),
                );
                let wait = inner.key_backoff.failed();
                inner.key_next_poll = Some(now + wait.min(self.timing.key_poll));
            }
        }
        inner.key.clone()
    }

    /// Gets a JWT when there is none and renews it at half its life. Returns whether a
    /// usable one is held.
    async fn ensure_credential(&self, now: Instant, hub: &HubView, key: &LeafKey) -> bool {
        let Some(fingerprint) = hub.fingerprint().map(str::to_string) else {
            return false;
        };
        {
            let inner = self.inner();
            let renewing = inner
                .credential
                .as_ref()
                .is_some_and(|credential| credential.renew_in((self.wall_clock)()) == 0);
            if (inner.credential.is_some() && !renewing) || !inner.issue_retry.due(now) {
                return inner.credential.is_some();
            }
        }
        let issued = match hub {
            HubView::SelfHub { .. } => self.deps.local_hub.issue(&key.nkey).await,
            _ => {
                self.deps
                    .issuer
                    .hub_leaf_issue(&fingerprint, &key.nkey)
                    .await
            }
        };
        let unix = (self.wall_clock)();
        let accepted = issued.and_then(|jwt| LeafCredential::accept(&jwt, key, &fingerprint, unix));
        let mut inner = self.inner();
        match accepted {
            Ok(credential) => {
                let replaces = inner.credential.is_some() || inner.replacing;
                let renew_in = credential.renew_in(unix);
                log_event(
                    "ckbus.federation.leaf_credential",
                    json!({
                        "hub": fingerprint,
                        "leaf_nkey": key.nkey,
                        "expires_at": credential.expires_at,
                        "renew_in_s": renew_in,
                        "replaces": replaces,
                    }),
                );
                inner.renew_due = Some(now + Duration::from_secs(renew_in));
                inner.credential = Some(credential);
                inner.issue_retry.succeeded();
                inner.replacing = false;
                if replaces {
                    // A kick already waiting keeps its deadline: the cap counts from the
                    // first JWT the live connection is behind on.
                    let deadline = now + self.timing.kick_cap;
                    inner.kick.get_or_insert(KickPlan { deadline });
                }
            }
            Err(reason) => {
                log_event(
                    "ckbus.federation.leaf_credential_failed",
                    json!({ "hub": fingerprint, "reason": reason }),
                );
                inner.issue_retry.failed(now);
            }
        }
        inner.credential.is_some()
    }

    /// Reads the leaf report when due, then opens the relay bridge after a failed round
    /// and bounces the link for a pending kick.
    async fn watch_link(&self, now: Instant, hub: &HubView, account: &str) {
        if self.inner().link.next_poll.is_some_and(|at| now < at) {
            return;
        }
        let report = self.deps.links.leafs(account).await;
        let open_bridge = {
            let mut inner = self.inner();
            inner.link.next_poll = Some(now + self.timing.link_poll);
            match report {
                Ok(links) => {
                    inner.link.read_failure_logged = false;
                    inner.link.connected = Some(!links.is_empty());
                    if links.is_empty() {
                        inner.link.down_since.get_or_insert(now);
                    } else {
                        inner.link.down_since = None;
                    }
                    inner.link.links = links;
                }
                Err(reason) => {
                    // An unreadable report says nothing about the link, so neither a
                    // bridge nor a kick is decided on it.
                    if !inner.link.read_failure_logged {
                        log_event(
                            "ckbus.federation.leaf_report_unavailable",
                            json!({ "reason": reason }),
                        );
                        inner.link.read_failure_logged = true;
                    }
                    inner.link.connected = None;
                }
            }
            self.bridge_due(&inner, now, hub)
        };
        if let (true, Some(fingerprint)) = (open_bridge, hub.fingerprint()) {
            let opened = self.deps.callosum.leaf_bridge_open(fingerprint).await;
            let mut inner = self.inner();
            inner.link.bridge_opened_at = Some(now);
            match opened {
                Ok(port) => {
                    log_event(
                        "ckbus.federation.bridge_opened",
                        json!({ "hub": fingerprint, "port": port }),
                    );
                    inner.bridge = Some(port);
                }
                Err(reason) => log_event(
                    "ckbus.federation.bridge_unavailable",
                    json!({ "hub": fingerprint, "reason": reason }),
                ),
            }
        }
        self.kick_if_due(now).await;
    }

    /// A peer hub gets a bridge once a whole round of its direct candidates has failed:
    /// each candidate given `round_per_url`, or at once with no candidates. A bridge that
    /// does not bring the link up within another round is reopened, since its port may
    /// have closed.
    fn bridge_due(&self, inner: &Inner, now: Instant, hub: &HubView) -> bool {
        let HubView::Peer { candidates, .. } = hub else {
            return false;
        };
        if inner.link.connected == Some(true) {
            return false;
        }
        let round = self.timing.round_per_url * u32::try_from(candidates.len().max(1)).unwrap_or(1);
        if candidates.is_empty() && inner.link.bridge_opened_at.is_none() {
            return true;
        }
        if inner.link.connected != Some(false) {
            return false;
        }
        let since = match inner.link.bridge_opened_at {
            Some(opened) => opened.max(inner.link.down_since.unwrap_or(opened)),
            None => match inner.link.down_since {
                Some(down) => down,
                None => return false,
            },
        };
        now.duration_since(since) >= round
    }

    /// Bounces the leaf connection once after its JWT was replaced, at a quiet moment
    /// or when the cap runs out. The server keeps the JWT a connection presented, so only
    /// a reconnect makes the engine sign with the new one.
    async fn kick_if_due(&self, now: Instant) {
        let links = {
            let mut inner = self.inner();
            let Some(plan) = inner.kick else { return };
            match inner.link.connected {
                // Down: its reconnect presents the new JWT anyway.
                Some(false) => {
                    inner.kick = None;
                    return;
                }
                None => return,
                Some(true) => {}
            }
            if !(self.deps.quiet.quiet_for(self.timing.quiet_window) || now >= plan.deadline) {
                return;
            }
            inner.kick = None;
            inner.link.links.clone()
        };
        for link in links {
            let result = self.deps.links.kick(&link).await;
            log_event(
                "ckbus.federation.leaf_kicked",
                json!({
                    "server_id": link.server_id,
                    "id": link.id,
                    "outcome": match &result { Ok(()) => "kicked".to_string(), Err(e) => e.clone() },
                }),
            );
        }
    }

    /// `ckbus.leaf_sign`: signs the hub's connect nonce for the remote named `remote` with
    /// the leaf key, and returns that remote's current JWT.
    pub async fn sign(&self, remote: &str, nonce_b64: &str) -> Result<Value, SignRefusal> {
        // nats-server's nonce is 11 random bytes in unpadded base64url: exactly 15
        // characters. Anything else is refused, so this op can never sign arbitrary data.
        let nonce_ok = nonce_b64.len() == 15
            && nonce_b64
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !nonce_ok {
            return Err(SignRefusal::new(
                sign_code::INVALID_NONCE,
                "nonce_b64 must be the 15-character base64url nonce nats-server sends",
            ));
        }
        let (jwt, key_id, expires_at) = {
            let known = self
                .published
                .borrow()
                .remotes
                .iter()
                .any(|r| r.name == remote);
            let inner = self.inner();
            if !known {
                return Err(SignRefusal::new(
                    sign_code::UNKNOWN_REMOTE,
                    format!("no current remote is named {remote:?}"),
                ));
            }
            let Some(credential) = inner.credential.as_ref() else {
                return Err(SignRefusal::new(
                    sign_code::NO_CREDENTIAL,
                    "no leaf JWT is held for the hub",
                ));
            };
            self.check_sign_expiry(credential.expires_at)?;
            (
                credential.jwt.clone(),
                credential.key_id.clone(),
                credential.expires_at,
            )
        };
        let signed = tokio::time::timeout(
            self.timing.sign_timeout,
            self.deps
                .vault
                .sign(&self.leaf_key_id, nonce_b64.as_bytes()),
        )
        .await;
        let signature = match signed {
            Ok(Ok(signature)) => signature,
            Ok(Err(error)) => {
                return Err(SignRefusal::new(
                    sign_code::VAULT_UNAVAILABLE,
                    error.to_string(),
                ))
            }
            Err(_) => {
                return Err(SignRefusal::new(
                    sign_code::VAULT_UNAVAILABLE,
                    format!(
                        "credential.sign took longer than {:?}",
                        self.timing.sign_timeout
                    ),
                ))
            }
        };
        if signature.key_id != key_id {
            // The key moved since the JWT was issued, so the hub would refuse this
            // signature. Re-read the key now; the next connect gets a new JWT.
            self.inner().key_next_poll = None;
            self.wake.notify_one();
            return Err(SignRefusal::new(
                sign_code::KEY_ROTATED,
                "the leaf key changed since its JWT was issued; a new JWT is being requested",
            ));
        }
        self.check_sign_expiry(expires_at)?;
        Ok(json!({
            "jwt": jwt,
            "signature_b64": URL_SAFE_NO_PAD.encode(signature.signature),
        }))
    }

    fn check_sign_expiry(&self, expires_at: i64) -> Result<(), SignRefusal> {
        if expires_at <= (self.wall_clock)().saturating_add(60) {
            self.wake.notify_one();
            return Err(SignRefusal::new(
                sign_code::NO_CREDENTIAL,
                "the leaf JWT expires within 60 seconds; a fresh JWT is needed",
            ));
        }
        Ok(())
    }
}

/// Follows `callosum.hub_watch`, and reads `callosum.hub_read` every `period` while the
/// watch is down. Reopening the watch is retried from 1 s, doubling up to `period`.
pub(crate) async fn follow_hub(
    callosum: Arc<dyn Callosum>,
    views: mpsc::Sender<HubView>,
    period: Duration,
) {
    let mut retry = Duration::from_secs(1).min(period);
    let mut next_read = Instant::now();
    loop {
        match callosum.hub_watch().await {
            Ok(mut notices) => {
                log_event("ckbus.federation.hub_watch", json!({ "state": "open" }));
                retry = Duration::from_secs(1).min(period);
                while let Some(body) = notices.recv().await {
                    if views.send(parse_hub_body(&body)).await.is_err() {
                        return;
                    }
                }
                log_event("ckbus.federation.hub_watch", json!({ "state": "ended" }));
                next_read = Instant::now();
            }
            Err(reason) => log_event(
                "ckbus.federation.hub_watch",
                json!({ "state": "unavailable", "reason": reason }),
            ),
        }
        let now = Instant::now();
        if now >= next_read {
            match callosum.hub_read().await {
                Ok(body) => {
                    if views.send(parse_hub_body(&body)).await.is_err() {
                        return;
                    }
                }
                // The last known hub stays in force; a failed read changes nothing.
                Err(reason) => log_event(
                    "ckbus.federation.hub_read_failed",
                    json!({ "reason": reason }),
                ),
            }
            next_read = Instant::now() + period;
        }
        tokio::time::sleep(retry.min(next_read.saturating_duration_since(Instant::now()))).await;
        retry = (retry * 2).min(period);
    }
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

pub(crate) fn log_event(event: &str, fields: Value) {
    let mut line = json!({ "event": event, "at_ms": unix_ms() });
    if let (Some(line), Value::Object(fields)) = (line.as_object_mut(), fields) {
        line.extend(fields);
    }
    eprintln!("{line}");
}
