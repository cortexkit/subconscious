//! What bootstrap does on the broker, behind two traits so the boot sequence can be
//! driven against a recording fake as well as a real `nats-server`.
//!
//! `SystemPlane` is ck-bus's system-account user: the claims list, lookup and update
//! served by the server's full (directory) resolver, the kick, and the connect and
//! disconnect events the kick targets are learned from. `BoxPlane` is ck-bus's
//! box-account user: stream creation, the sentinel subject and the census bucket.
//!
//! A claims update is saved by the server without checking that its issuer is trusted
//! or that its `iat` is newer, so the update's own reply proves nothing about the
//! account the server will load. `apply_account_jwt` therefore reads every push back
//! through the lookup and treats anything but the exact pushed token as not applied.

use std::{fmt, sync::Arc, time::Duration};

use async_nats::jetstream::{self, kv, stream};
use async_trait::async_trait;
use cortexkit_bus_naming::{AccountNames, DiscardPolicy, StreamSpec, MIB};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};

use crate::credentials::Credentials;

/// Server subjects the resolver and the server's system services answer on. The grant
/// that allows each comes from `cortexkit-bus-naming`'s `system_permissions`.
const CLAIMS_UPDATE: &str = "$SYS.REQ.CLAIMS.UPDATE";
const CLAIMS_LIST: &str = "$SYS.REQ.CLAIMS.LIST";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How many server error lines a slow reader of `SentinelLink::server_errors` may fall
/// behind by before the oldest are dropped. A probe reads them within one round, and a
/// round sees at most a handful.
const SERVER_ERROR_BACKLOG: usize = 64;

/// The census bucket's size cap. The foundation fixes history 1 and no TTL but names no
/// size, and a stream with no stated size is unbounded, so ck-bus states one. One value
/// per live module process, each well under 1 KiB (two keys, a jti, two counters and
/// the bound agent and room lists), so 16 MiB holds tens of thousands of live processes,
/// far past one machine's supervisor. The stream discards NEW when full: a census write
/// that does not fit is refused, and issuance fails loudly, instead of the server
/// evicting a live process's entry, which would read as that process being revoked.
pub const CENSUS_MAX_BYTES: i64 = 16 * MIB as i64;

/// One stored census value and the KV revision it is stored at. The revision is what a
/// compare-and-delete names, so a value overwritten since it was read is never deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CensusRecord {
    pub value: Vec<u8>,
    pub revision: u64,
}

/// One client connect or disconnect in the box account, from the server's `$SYS`
/// account events. `user` is the connecting user's public key (`U...`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionEvent {
    Connected {
        server_id: String,
        client_id: u64,
        user: String,
    },
    Disconnected {
        server_id: String,
        client_id: u64,
        user: String,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaneError {
    pub message: String,
}

impl PlaneError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for PlaneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

#[async_trait]
pub trait SystemPlane: Send + Sync {
    /// Every account id the resolver has stored.
    async fn list_accounts(&self) -> Result<Vec<String>, PlaneError>;
    /// The stored account JWT, `None` when the resolver answers that it holds none.
    async fn lookup(&self, account_public: &str) -> Result<Option<String>, PlaneError>;
    /// Pushes an account JWT. `Ok` means the server answered the update; it does NOT
    /// mean the JWT is trusted or current (see `apply_account_jwt`).
    async fn update(&self, jwt: &str) -> Result<(), PlaneError>;
    /// Disconnects one client connection on one server.
    async fn kick(&self, server_id: &str, client_id: u64) -> Result<(), PlaneError>;
    /// Subscribes to the connect and disconnect events of `account_public`'s clients.
    /// Only connections made while the subscription is open are seen: a client that
    /// connected earlier appears first in its disconnect event.
    async fn watch_connections(
        &self,
        account_public: &str,
    ) -> Result<mpsc::UnboundedReceiver<ConnectionEvent>, PlaneError>;
}

#[async_trait]
pub trait BoxPlane: Send + Sync {
    /// Creates the census bucket's backing stream if absent, and brings an existing one
    /// to the configuration below (its messages are kept).
    async fn ensure_census(&self, account: &AccountNames) -> Result<(), PlaneError>;
    async fn ensure_stream(&self, spec: &StreamSpec) -> Result<(), PlaneError>;
    async fn publish(&self, subject: &str, payload: Vec<u8>) -> Result<(), PlaneError>;
    /// Writes one census record: a JetStream publish on the census key's KV subject
    /// (`AccountNames::census_subject`), answered only once the census stream has stored
    /// it. The bucket keeps one value per key, so this overwrites the module's entry.
    async fn census_put(&self, subject: &str, value: Vec<u8>) -> Result<(), PlaneError>;
    /// Reads one census key (`AccountNames::census_key`). `Ok(None)` is the bucket's own
    /// answer that the key holds no value (never written, or deleted); a read that fails
    /// is an `Err`, never `None`.
    async fn census_get(
        &self,
        account: &AccountNames,
        key: &str,
    ) -> Result<Option<CensusRecord>, PlaneError>;
    /// Every census key that holds a value now (a deleted key is not listed). A listing
    /// that fails is an `Err`, never an empty list. A plane that cannot list (a test
    /// double that wraps only the calls it records) answers `Err` by default, which
    /// the spawn consumer's reconciliation reads as "unknown" and defers on.
    async fn census_keys(&self, _account: &AccountNames) -> Result<Vec<String>, PlaneError> {
        Err(PlaneError::new("this census plane cannot list keys"))
    }
    /// Deletes one census key only while it is still at `revision`. A key written again
    /// since that revision is left as it is and the call fails.
    async fn census_delete(
        &self,
        account: &AccountNames,
        key: &str,
        revision: u64,
    ) -> Result<(), PlaneError>;
    /// Creates a participant's durable pull consumer, or updates it to `durable`'s
    /// configuration when it already exists.
    async fn ensure_durable(&self, durable: &DurableConsumer) -> Result<(), PlaneError>;
    /// The connection itself, for the sentinel probe's request and responder. A plane
    /// with no real connection (a test double) has none, and the sentinel reports that
    /// it cannot probe.
    fn sentinel_link(&self) -> Option<SentinelLink> {
        None
    }
}

/// ck-bus's box-account connection as the sentinel probe uses it. The server reports a
/// permissions violation only as an error line on the connection, never to the request
/// that caused it, so every such line is forwarded on `server_errors` for the probe to
/// read.
#[derive(Clone)]
pub struct SentinelLink {
    pub client: async_nats::Client,
    /// The connection's user, which names its inbox prefix `_INBOX.<user_public>`.
    pub user_public: String,
    pub server_errors: broadcast::Sender<String>,
}

/// One participant durable consumer, with every limit stated rather than left to a
/// server default. The foundation fixes pull, explicit ack, ack wait 30 s, max-deliver 5
/// on the effect stream and unlimited (`-1`) elsewhere; it names no ack-pending cap, so
/// the caller sets one explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableConsumer {
    pub stream: String,
    pub durable: String,
    pub filter_subjects: Vec<String>,
    pub ack_wait: Duration,
    pub max_deliver: i64,
    pub max_ack_pending: i64,
}

/// Connects ck-bus's own users. Each connection answers the server's nonce with the
/// user's seed held in ck-bus's memory.
#[async_trait]
pub trait Broker: Send + Sync {
    async fn connect_system(
        &self,
        jwt: &str,
        user_public: &str,
    ) -> Result<Arc<dyn SystemPlane>, PlaneError>;
    async fn connect_box(
        &self,
        jwt: &str,
        user_public: &str,
    ) -> Result<Arc<dyn BoxPlane>, PlaneError>;
}

/// Why a pushed account JWT is not treated as applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    Plane(PlaneError),
    /// The lookup after the push returned something other than the pushed token.
    ReadBackMismatch {
        account_public: String,
        pushed_jti: String,
        read_back: String,
    },
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plane(error) => error.fmt(f),
            Self::ReadBackMismatch {
                account_public,
                pushed_jti,
                read_back,
            } => write!(
                f,
                "claims update for {account_public} not applied: pushed jti {pushed_jti}, \
                 read back {read_back}"
            ),
        }
    }
}

/// Pushes `jwt` and reads it back. Only an exact read-back counts as applied.
pub async fn apply_account_jwt(
    plane: &dyn SystemPlane,
    account_public: &str,
    jwt: &str,
) -> Result<(), ApplyError> {
    plane.update(jwt).await.map_err(ApplyError::Plane)?;
    let read_back = plane
        .lookup(account_public)
        .await
        .map_err(ApplyError::Plane)?;
    if read_back.as_deref().map(str::trim) == Some(jwt) {
        return Ok(());
    }
    let jti = |token: &str| {
        super::account_jwt::decode_claims(token)
            .and_then(|claims| claims["jti"].as_str().map(str::to_string))
            .unwrap_or_else(|| "an undecodable token".to_string())
    };
    Err(ApplyError::ReadBackMismatch {
        account_public: account_public.to_string(),
        pushed_jti: jti(jwt),
        read_back: match read_back {
            None => "nothing (the resolver holds no JWT for the account)".to_string(),
            Some(token) => format!("jti {}", jti(&token)),
        },
    })
}

/// The real broker: `async-nats` against the local `nats-server`.
pub struct NatsBroker {
    url: String,
    credentials: Arc<Credentials>,
}

impl NatsBroker {
    pub fn new(url: String, credentials: Arc<Credentials>) -> Self {
        Self { url, credentials }
    }

    async fn connect(
        &self,
        jwt: &str,
        user_public: &str,
        name: &str,
        server_errors: broadcast::Sender<String>,
    ) -> Result<async_nats::Client, PlaneError> {
        let credentials = self.credentials.clone();
        let user = user_public.to_string();
        async_nats::ConnectOptions::with_jwt(jwt.to_string(), move |nonce| {
            let credentials = credentials.clone();
            let user = user.clone();
            async move {
                credentials
                    .custody
                    .sign_nonce(&user, &nonce)
                    .map_err(|superseded| async_nats::AuthError::new(superseded.to_string()))
            }
        })
        // Replies come back on the user's own inbox, the only inbox its grant allows.
        .custom_inbox_prefix(format!("_INBOX.{user_public}"))
        .event_callback(move |event| {
            let server_errors = server_errors.clone();
            async move {
                if let async_nats::Event::ServerError(async_nats::ServerError::Other(line)) = event
                {
                    // Nobody listening is normal; the line is only for a probe in flight.
                    let _ = server_errors.send(line);
                }
            }
        })
        .name(name)
        .connection_timeout(REQUEST_TIMEOUT)
        .connect(&self.url)
        .await
        .map_err(|error| PlaneError::new(format!("connect as {name} to {}: {error}", self.url)))
    }
}

#[async_trait]
impl Broker for NatsBroker {
    async fn connect_system(
        &self,
        jwt: &str,
        user_public: &str,
    ) -> Result<Arc<dyn SystemPlane>, PlaneError> {
        let (server_errors, _) = broadcast::channel(SERVER_ERROR_BACKLOG);
        let client = self
            .connect(jwt, user_public, "ckbus-system", server_errors)
            .await?;
        Ok(Arc::new(NatsSystem { client }))
    }

    async fn connect_box(
        &self,
        jwt: &str,
        user_public: &str,
    ) -> Result<Arc<dyn BoxPlane>, PlaneError> {
        let (server_errors, _) = broadcast::channel(SERVER_ERROR_BACKLOG);
        let client = self
            .connect(jwt, user_public, "ckbus-box", server_errors.clone())
            .await?;
        let jetstream = jetstream::new(client.clone());
        let link = SentinelLink {
            client: client.clone(),
            user_public: user_public.to_string(),
            server_errors,
        };
        Ok(Arc::new(NatsBox {
            client,
            jetstream,
            link,
        }))
    }
}

pub struct NatsSystem {
    client: async_nats::Client,
}

impl NatsSystem {
    async fn request(&self, subject: String, payload: Vec<u8>) -> Result<Vec<u8>, PlaneError> {
        let reply = tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.client.request(subject.clone(), payload.into()),
        )
        .await
        .map_err(|_| PlaneError::new(format!("{subject}: no reply within {REQUEST_TIMEOUT:?}")))?
        .map_err(|error| PlaneError::new(format!("{subject}: {error}")))?;
        Ok(reply.payload.to_vec())
    }
}

#[async_trait]
impl SystemPlane for NatsSystem {
    async fn list_accounts(&self) -> Result<Vec<String>, PlaneError> {
        let body = self.request(CLAIMS_LIST.to_string(), Vec::new()).await?;
        let value: Value = serde_json::from_slice(&body).map_err(|error| {
            PlaneError::new(format!("{CLAIMS_LIST} reply is not JSON: {error}"))
        })?;
        if let Some(error) = value.get("error") {
            return Err(PlaneError::new(format!("{CLAIMS_LIST} refused: {error}")));
        }
        value["data"]
            .as_array()
            .ok_or_else(|| PlaneError::new(format!("{CLAIMS_LIST} reply has no data list")))?
            .iter()
            .map(|id| {
                id.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| PlaneError::new(format!("{CLAIMS_LIST} listed a non-string")))
            })
            .collect()
    }

    async fn lookup(&self, account_public: &str) -> Result<Option<String>, PlaneError> {
        let body = self
            .request(
                format!("$SYS.REQ.ACCOUNT.{account_public}.CLAIMS.LOOKUP"),
                Vec::new(),
            )
            .await?;
        // The directory resolver answers an empty body for an account it holds no JWT
        // for, which is its own confirmed absence.
        if body.is_empty() {
            return Ok(None);
        }
        String::from_utf8(body)
            .map(Some)
            .map_err(|_| PlaneError::new("claims lookup reply is not UTF-8"))
    }

    async fn update(&self, jwt: &str) -> Result<(), PlaneError> {
        let body = self
            .request(CLAIMS_UPDATE.to_string(), jwt.as_bytes().to_vec())
            .await?;
        let value: Value = serde_json::from_slice(&body).map_err(|error| {
            PlaneError::new(format!("{CLAIMS_UPDATE} reply is not JSON: {error}"))
        })?;
        if let Some(error) = value.get("error") {
            return Err(PlaneError::new(format!("{CLAIMS_UPDATE} refused: {error}")));
        }
        Ok(())
    }

    async fn kick(&self, server_id: &str, client_id: u64) -> Result<(), PlaneError> {
        let body = self
            .request(
                format!("$SYS.REQ.SERVER.{server_id}.KICK"),
                serde_json::to_vec(&json!({"cid": client_id})).map_err(|error| {
                    PlaneError::new(format!("kick request does not encode: {error}"))
                })?,
            )
            .await?;
        let value: Value = serde_json::from_slice(&body)
            .map_err(|error| PlaneError::new(format!("kick reply is not JSON: {error}")))?;
        match value.get("error") {
            Some(error) => Err(PlaneError::new(format!("kick refused: {error}"))),
            None => Ok(()),
        }
    }

    async fn watch_connections(
        &self,
        account_public: &str,
    ) -> Result<mpsc::UnboundedReceiver<ConnectionEvent>, PlaneError> {
        let mut subscriptions = Vec::new();
        for kind in ["CONNECT", "DISCONNECT"] {
            let subject = format!("$SYS.ACCOUNT.{account_public}.{kind}");
            subscriptions.push(
                self.client
                    .subscribe(subject.clone())
                    .await
                    .map_err(|error| PlaneError::new(format!("subscribe {subject}: {error}")))?,
            );
        }
        // The subscriptions are registered with the server once this flush returns, so
        // every connect after it is seen.
        self.client
            .flush()
            .await
            .map_err(|error| PlaneError::new(format!("flush connection watch: {error}")))?;
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut merged = futures_util::stream::select_all(subscriptions);
        tokio::spawn(async move {
            while let Some(message) = merged.next().await {
                if let Some(event) = parse_connection_event(&message.payload) {
                    if sender.send(event).is_err() {
                        return;
                    }
                }
            }
        });
        Ok(receiver)
    }
}

/// Reads one `$SYS` account connect or disconnect event. The server names the connection
/// by its server id and client id (`cid`), which is what a kick addresses, and the user
/// by the public key it authenticated with.
pub fn parse_connection_event(payload: &[u8]) -> Option<ConnectionEvent> {
    let value: Value = serde_json::from_slice(payload).ok()?;
    let server_id = value["server"]["id"].as_str()?.to_string();
    let client = &value["client"];
    let client_id = client["id"].as_u64()?;
    let user = client["user"]
        .as_str()
        .filter(|user| user.starts_with('U'))
        .or_else(|| client["nkey"].as_str())?
        .to_string();
    match value["type"].as_str()? {
        "io.nats.server.advisory.v1.client_connect" => Some(ConnectionEvent::Connected {
            server_id,
            client_id,
            user,
        }),
        "io.nats.server.advisory.v1.client_disconnect" => Some(ConnectionEvent::Disconnected {
            server_id,
            client_id,
            user,
            reason: value["reason"].as_str().unwrap_or_default().to_string(),
        }),
        _ => None,
    }
}

pub struct NatsBox {
    client: async_nats::Client,
    jetstream: jetstream::Context,
    link: SentinelLink,
}

#[async_trait]
impl BoxPlane for NatsBox {
    async fn ensure_census(&self, account: &AccountNames) -> Result<(), PlaneError> {
        let buckets = account.buckets();
        // The census is a KV bucket: history 1, no TTL. It is created as its backing
        // stream directly, because the client library's bucket helper first reads the
        // account's JetStream info, which ck-bus's grant does not allow.
        // Every limit is stated: history 1 and no TTL (a zero max age) are the
        // foundation's; the size cap is `CENSUS_MAX_BYTES`; message and consumer counts
        // are unlimited (-1) because history 1 bounds the first and every participant's
        // census watch is a consumer.
        let config = stream::Config {
            name: buckets.census_stream.clone(),
            subjects: vec![format!("$KV.{}.>", buckets.census)],
            max_messages_per_subject: 1,
            max_bytes: CENSUS_MAX_BYTES,
            max_messages: -1,
            max_consumers: -1,
            max_age: Duration::ZERO,
            allow_rollup: true,
            deny_delete: true,
            allow_direct: true,
            discard: stream::DiscardPolicy::New,
            storage: stream::StorageType::File,
            num_replicas: 1,
            ..Default::default()
        };
        // Update first, create when absent: a census stream an earlier ck-bus created
        // without the size cap is brought to it rather than refused as a configuration
        // mismatch, and its values are kept.
        let name = config.name.clone();
        self.jetstream
            .create_or_update_stream(config)
            .await
            .map(|_| ())
            .map_err(|error| PlaneError::new(format!("create or update stream {name}: {error}")))
    }

    async fn ensure_stream(&self, spec: &StreamSpec) -> Result<(), PlaneError> {
        let config = stream::Config {
            name: spec.name.clone(),
            subjects: spec.subjects.clone(),
            max_age: spec.max_age,
            max_bytes: i64::try_from(spec.max_bytes).unwrap_or(i64::MAX),
            discard: match spec.discard {
                DiscardPolicy::Old => stream::DiscardPolicy::Old,
                DiscardPolicy::New => stream::DiscardPolicy::New,
            },
            retention: if spec.work_queue {
                stream::RetentionPolicy::WorkQueue
            } else {
                stream::RetentionPolicy::Limits
            },
            storage: stream::StorageType::File,
            num_replicas: 1,
            ..Default::default()
        };
        self.create(config).await
    }

    async fn publish(&self, subject: &str, payload: Vec<u8>) -> Result<(), PlaneError> {
        self.client
            .publish(subject.to_string(), payload.into())
            .await
            .map_err(|error| PlaneError::new(format!("publish {subject}: {error}")))?;
        self.client
            .flush()
            .await
            .map_err(|error| PlaneError::new(format!("flush after {subject}: {error}")))
    }

    async fn census_put(&self, subject: &str, value: Vec<u8>) -> Result<(), PlaneError> {
        let ack = self
            .jetstream
            .publish(subject.to_string(), value.into())
            .await
            .map_err(|error| PlaneError::new(format!("census put {subject}: {error}")))?;
        ack.await
            .map(|_| ())
            .map_err(|error| PlaneError::new(format!("census put {subject} not stored: {error}")))
    }

    async fn census_get(
        &self,
        account: &AccountNames,
        key: &str,
    ) -> Result<Option<CensusRecord>, PlaneError> {
        let entry = self
            .census_store(account)
            .await?
            .entry(key)
            .await
            .map_err(|error| PlaneError::new(format!("census get {key}: {error}")))?;
        Ok(entry
            .filter(|entry| entry.operation == kv::Operation::Put)
            .map(|entry| CensusRecord {
                value: entry.value.to_vec(),
                revision: entry.revision,
            }))
    }

    async fn census_keys(&self, account: &AccountNames) -> Result<Vec<String>, PlaneError> {
        let mut keys = self
            .census_store(account)
            .await?
            .keys()
            .await
            .map_err(|error| PlaneError::new(format!("census keys: {error}")))?;
        let mut listed = Vec::new();
        while let Some(key) = keys.next().await {
            listed.push(key.map_err(|error| PlaneError::new(format!("census keys: {error}")))?);
        }
        Ok(listed)
    }

    async fn census_delete(
        &self,
        account: &AccountNames,
        key: &str,
        revision: u64,
    ) -> Result<(), PlaneError> {
        self.census_store(account)
            .await?
            .delete_expect_revision(key, Some(revision))
            .await
            .map_err(|error| {
                PlaneError::new(format!(
                    "census delete {key} at revision {revision}: {error}"
                ))
            })
    }

    fn sentinel_link(&self) -> Option<SentinelLink> {
        Some(self.link.clone())
    }

    async fn ensure_durable(&self, durable: &DurableConsumer) -> Result<(), PlaneError> {
        let config = jetstream::consumer::pull::Config {
            durable_name: Some(durable.durable.clone()),
            ack_policy: jetstream::consumer::AckPolicy::Explicit,
            ack_wait: durable.ack_wait,
            max_deliver: durable.max_deliver,
            max_ack_pending: durable.max_ack_pending,
            filter_subjects: durable.filter_subjects.clone(),
            ..Default::default()
        };
        self.jetstream
            .create_consumer_on_stream(config, durable.stream.as_str())
            .await
            .map(|_| ())
            .map_err(|error| {
                PlaneError::new(format!(
                    "create durable {} on {}: {error}",
                    durable.durable, durable.stream
                ))
            })
    }
}

impl NatsBox {
    /// The census bucket. Binding to it reads the census stream's info, which the
    /// bus-module grant allows; the bucket itself is created by `ensure_census`.
    async fn census_store(&self, account: &AccountNames) -> Result<kv::Store, PlaneError> {
        let bucket = account.buckets().census.clone();
        self.jetstream
            .get_key_value(bucket.clone())
            .await
            .map_err(|error| PlaneError::new(format!("census bucket {bucket}: {error}")))
    }

    /// `STREAM.CREATE` is idempotent for an identical configuration, so creating on
    /// every boot is "create if absent" and never touches the stream's messages.
    async fn create(&self, config: stream::Config) -> Result<(), PlaneError> {
        let name = config.name.clone();
        self.jetstream
            .create_stream(config)
            .await
            .map(|_| ())
            .map_err(|error| PlaneError::new(format!("create stream {name}: {error}")))
    }
}
