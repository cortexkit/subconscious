//! The hub area's production seams: callosum's paired-device ops over ck-bus's own subc
//! client connection, and the hub instance through ck-bus's own users on its loopback
//! admin listener.
//!
//! Those users (one in the hub system account, one in the hub account) get nkeys
//! generated in memory once per process, never written anywhere. Each connection
//! presents a freshly signed 15-minute JWT and is reused for at most 10 minutes, then
//! replaced by a new one, so no JWT is ever renewed in place and the vault signs at
//! most a few times an hour for them.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_nats::jetstream;
use async_trait::async_trait;
use cortexkit_bus_naming::federation::HubName;
use futures_util::TryStreamExt as _;
use serde_json::json;
use subc_client_rs::consumer::SubscribeOptions;
use subc_protocol::RouteTarget;
use tokio::sync::{broadcast, mpsc, watch, Mutex, OnceCell};

use super::{
    area::{HubArea, HubDeps, HubFiles, HubTiming},
    conf::admin_url,
    jwt::{
        admin_user_permissions, sign_claims, system_user_permissions, UserJwt,
        ADMIN_JWT_LIFETIME_SECS,
    },
    record::HubRecord,
    seams::{HubServer, HubStream, OwnMachine, PairedDevices},
    streams::config_for,
    HUB_SERVER_MODULE,
};
use crate::{
    bootstrap::{
        config::NATS_URL_ENV,
        plane::{apply_account_jwt, reconcile_stream_config, NatsBroker, NatsSystem, SystemPlane},
        Ready,
    },
    credentials::{nkey::NkeyRole, roots::RootCredential, Credentials},
    federation::leaf::{
        area::log_event,
        production::{body, describe, SubcFederation, CALLOSUM_MODULE_ID},
    },
};

#[async_trait]
impl PairedDevices for SubcFederation {
    async fn watch(&self) -> Result<mpsc::Receiver<Vec<u8>>, String> {
        let consumer = self.consumer().await?;
        let method = "callosum.paired_devices_watch";
        let mut subscription = consumer
            .subscribe(
                RouteTarget::ManagementSurface {
                    module_id: CALLOSUM_MODULE_ID.to_string(),
                },
                self.bind.clone(),
                body(method, json!({})),
                SubscribeOptions::default(),
            )
            .await
            .map_err(|error| describe(CALLOSUM_MODULE_ID, method, &error))?;
        let (notices, receiver) = mpsc::channel(16);
        tokio::spawn(async move {
            while let Some(notice) = subscription.events().recv().await {
                if notices.send(notice).await.is_err() {
                    return;
                }
            }
        });
        Ok(receiver)
    }

    async fn read(&self) -> Result<Vec<u8>, String> {
        let value = self
            .call(
                CALLOSUM_MODULE_ID,
                "callosum.paired_devices_read",
                json!({}),
            )
            .await?;
        Ok(serde_json::to_vec(&value).expect("a JSON value encodes"))
    }

    async fn peer_keys(&self) -> Result<Vec<u8>, String> {
        let value = self
            .call(CALLOSUM_MODULE_ID, "callosum.peer_keys_read", json!({}))
            .await?;
        Ok(serde_json::to_vec(&value).expect("a JSON value encodes"))
    }
}

/// The hub instance through its admin listener.
pub struct NatsHubServer {
    url: String,
    credentials: Arc<Credentials>,
    system_user: OnceCell<String>,
    admin_user: OnceCell<String>,
    system_connection: Mutex<Option<Cached>>,
    admin_connection: Mutex<Option<Cached>>,
}

/// An open connection, the account it signed into, and when.
struct Cached {
    client: async_nats::Client,
    account: String,
    opened: std::time::Instant,
}

/// How long a connection is reused: well inside its JWT's 15 minutes, after which the
/// server would close it.
const CONNECTION_REUSE: Duration = Duration::from_secs(10 * 60);

impl Cached {
    fn usable(&self, account: &str) -> bool {
        self.account == account
            && self.opened.elapsed() < CONNECTION_REUSE
            && self.client.connection_state() == async_nats::connection::State::Connected
    }
}

impl NatsHubServer {
    pub fn new(url: String, credentials: Arc<Credentials>) -> Self {
        Self {
            url,
            credentials,
            system_user: OnceCell::new(),
            admin_user: OnceCell::new(),
            system_connection: Mutex::new(None),
            admin_connection: Mutex::new(None),
        }
    }

    /// Signs a short-lived JWT for one of ck-bus's own users on the hub.
    async fn user_jwt(
        &self,
        root: RootCredential,
        user_public: &str,
        account: &str,
        name: &str,
        (publish, subscribe): (Vec<String>, Vec<String>),
    ) -> Result<String, String> {
        let credential_id = root.credential_id().map_err(|error| error.to_string())?;
        let issued_at = unix_secs();
        let user = UserJwt {
            user_public,
            issuer_account: account,
            name,
            issued_at,
            expires_at: issued_at + ADMIN_JWT_LIFETIME_SECS,
            publish: &publish,
            subscribe: &subscribe,
        };
        Ok(sign_claims(
            self.credentials.vault.as_ref(),
            &credential_id,
            NkeyRole::Account,
            |issuer| user.claims(issuer),
        )
        .await?
        .jwt)
    }

    /// A system-account connection. The system account lists the hub operator key as
    /// its signing key, so the operator key signs this user.
    async fn system(&self, record: &HubRecord) -> Result<NatsSystem, String> {
        let mut cached = self.system_connection.lock().await;
        if let Some(open) = cached
            .as_ref()
            .filter(|open| open.usable(&record.system_account))
        {
            return Ok(NatsSystem::from_client(open.client.clone()));
        }
        let user = self
            .system_user
            .get_or_init(|| async { self.credentials.custody.generate_user() })
            .await
            .clone();
        let jwt = self
            .user_jwt(
                RootCredential::BusHubOperator,
                &user,
                &record.system_account,
                "ckbus-hub-system",
                system_user_permissions(&user),
            )
            .await?;
        let client = self.connect(&jwt, &user, "ckbus-hub-system").await?;
        *cached = Some(Cached {
            client: client.clone(),
            account: record.system_account.clone(),
            opened: std::time::Instant::now(),
        });
        Ok(NatsSystem::from_client(client))
    }

    /// A hub-account connection for the JetStream API.
    async fn admin(&self, record: &HubRecord) -> Result<async_nats::Client, String> {
        let mut cached = self.admin_connection.lock().await;
        if let Some(open) = cached
            .as_ref()
            .filter(|open| open.usable(&record.hub_account))
        {
            return Ok(open.client.clone());
        }
        let user = self
            .admin_user
            .get_or_init(|| async { self.credentials.custody.generate_user() })
            .await
            .clone();
        let jwt = self
            .user_jwt(
                RootCredential::BusHubAccount,
                &user,
                &record.hub_account,
                "ckbus-hub-admin",
                admin_user_permissions(&user),
            )
            .await?;
        let client = self.connect(&jwt, &user, "ckbus-hub-admin").await?;
        *cached = Some(Cached {
            client: client.clone(),
            account: record.hub_account.clone(),
            opened: std::time::Instant::now(),
        });
        Ok(client)
    }

    async fn connect(
        &self,
        jwt: &str,
        user: &str,
        name: &str,
    ) -> Result<async_nats::Client, String> {
        let (server_errors, _) = broadcast::channel(16);
        NatsBroker::new(self.url.clone(), self.credentials.clone())
            .connect(jwt, user, name, server_errors)
            .await
            .map(|(client, _)| client)
            .map_err(|error| error.message)
    }
}

/// Release the legacy or previous pin's subjects before assigning them to the new
/// inbox. No leaf grant allows publishing on the retired stream's own name.
async fn retire_inbox(context: &jetstream::Context, name: &str) -> Result<bool, String> {
    let mut stream = context
        .get_stream(name)
        .await
        .map_err(|error| format!("stream info {name}: {error}"))?;
    let mut config = stream.cached_info().config.clone();
    let detached = config.subjects != [name];
    if detached {
        config.subjects = vec![name.to_owned()];
        context
            .update_stream(config)
            .await
            .map_err(|error| format!("retire {name}: {error}"))?;
    }
    let messages = stream
        .info()
        .await
        .map_err(|error| format!("stream info {name}: {error}"))?
        .state
        .messages;
    if messages == 0 {
        let reply = context
            .delete_stream(name)
            .await
            .map_err(|error| format!("delete {name}: {error}"))?;
        if !reply.success {
            return Err(format!("delete {name} refused"));
        }
        log_event(
            "ckbus.federation.hub_inbox_retired",
            json!({"stream": name, "messages": 0}),
        );
    } else if detached {
        let legacy = name.len() == "HUB_INBOX_".len() + 32;
        log_event(
            "ckbus.federation.hub_inbox_retained",
            json!({"stream": name, "messages": messages, "level": if legacy { "ERROR" } else { "INFO" }, "legacy": legacy}),
        );
    }
    Ok(messages != 0)
}

#[async_trait]
impl HubServer for NatsHubServer {
    async fn apply_account(
        &self,
        record: &HubRecord,
        account_public: &str,
        jwt: &str,
    ) -> Result<(), String> {
        let system = self.system(record).await?;
        apply_account_jwt(&system, account_public, jwt)
            .await
            .map_err(|error| error.to_string())
    }

    async fn ensure_streams(
        &self,
        record: &HubRecord,
        streams: &[HubStream],
    ) -> Result<BTreeMap<String, Vec<HubName>>, String> {
        let client = self.admin(record).await?;
        let context = jetstream::new(client);
        let mut draining = BTreeMap::new();
        let names: Vec<String> = if streams
            .iter()
            .any(|spec| matches!(spec, HubStream::Inbox { .. }))
        {
            context
                .stream_names()
                .try_collect()
                .await
                .map_err(|error| error.to_string())?
        } else {
            Vec::new()
        };
        for hub_stream in streams {
            let mut config = config_for(hub_stream)?;
            if let HubStream::Inbox { box_id, hub } = hub_stream {
                let legacy = format!("HUB_INBOX_{box_id}");
                let prefix = format!("{legacy}_");
                let mut previous = Vec::new();
                for name in &names {
                    if name == &legacy {
                        retire_inbox(&context, name).await?;
                    } else if let Some(other) = name
                        .strip_prefix(&prefix)
                        .and_then(|suffix| HubName::parse(suffix).ok())
                    {
                        if &other != hub && retire_inbox(&context, name).await? {
                            previous.push(other);
                        }
                    }
                }
                previous.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
                draining.insert(box_id.clone(), previous);
            }
            // Naming uses zero for unlimited bytes; NATS reports the same limit as -1.
            if config.max_bytes == 0 {
                config.max_bytes = -1;
            }
            let name = config.name.clone();
            match context.get_stream(&name).await {
                Ok(existing) => {
                    let current = existing.cached_info().config.clone();
                    if current.storage != config.storage
                        || current.retention != config.retention
                        || (current.deny_delete && !config.deny_delete)
                    {
                        log_event(
                            "ckbus.stream.update_refused",
                            json!({
                                "level": "WARN", "stream": name,
                                "reason": "immutable stream settings differ",
                                "storage": current.storage, "desired_storage": config.storage,
                                "retention": current.retention, "desired_retention": config.retention,
                                "deny_delete": current.deny_delete, "desired_deny_delete": config.deny_delete,
                            }),
                        );
                        continue;
                    }
                    reconcile_stream_config(
                        &context,
                        current,
                        |current| {
                            current.subjects = config.subjects;
                            current.max_age = config.max_age;
                            current.max_bytes = config.max_bytes;
                            current.discard = config.discard;
                            if config.max_messages_per_subject != 0 {
                                current.max_messages_per_subject = config.max_messages_per_subject;
                            }
                            if config.max_message_size != 0 {
                                current.max_message_size = config.max_message_size;
                            }
                            if config.max_consumers != 0 {
                                // A lower cap gates new consumers without evicting existing ones.
                                current.max_consumers = config.max_consumers;
                            }
                            if !config.duplicate_window.is_zero() {
                                current.duplicate_window = config.duplicate_window;
                            }
                            current.num_replicas = config.num_replicas;
                            current.allow_rollup = config.allow_rollup;
                            current.allow_direct = config.allow_direct;
                            current.deny_delete = config.deny_delete;
                        },
                        |_| {},
                    )
                    .await
                    .map_err(|error| error.message)?;
                    continue;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        jetstream::context::GetStreamErrorKind::JetStream(ref api)
                            if api.error_code() == jetstream::ErrorCode::STREAM_NOT_FOUND
                    ) => {}
                Err(error) => return Err(format!("stream info {name}: {error}")),
            }
            context
                .create_stream(config)
                .await
                .map_err(|error| format!("create {name}: {error}"))?;
        }
        Ok(draining)
    }

    async fn leaf_count(&self, record: &HubRecord) -> Result<usize, String> {
        let system = self.system(record).await?;
        // The system connection must have learned the server's id first.
        for _ in 0..20 {
            if system.server_id().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        system
            .leafz(&record.hub_account)
            .await
            .map(|links| links.len())
            .map_err(|error| error.message)
    }
}

/// This machine's id, once bootstrap is ready.
pub struct ReadyOwnMachine {
    pub ready: watch::Receiver<Option<Arc<Ready>>>,
}

#[async_trait]
impl OwnMachine for ReadyOwnMachine {
    async fn machine_id(&self) -> Result<String, String> {
        self.ready
            .borrow()
            .as_ref()
            .map(|ready| ready.account.machine_id.clone())
            .ok_or_else(|| "bootstrap has not finished".to_string())
    }
}

/// Starts the hub area with its production seams. It shares the leaf area's callosum
/// connection; nothing is set up until callosum names this computer as the hub.
pub fn start(
    federation: Arc<SubcFederation>,
    credentials: Arc<Credentials>,
    store_root: &std::path::Path,
    ready: watch::Receiver<Option<Arc<Ready>>>,
) -> Arc<HubArea> {
    let area = HubArea::new(
        HubDeps {
            callosum: federation.clone(),
            devices: federation,
            vault: credentials.vault.clone(),
            server: Arc::new(NatsHubServer::new(admin_url(), credentials.clone())),
            own: Arc::new(ReadyOwnMachine { ready }),
            files: HubFiles {
                store_root: store_root.to_path_buf(),
                hub_dir: PathBuf::from(cortexkit_store_types::module_data_dir(HUB_SERVER_MODULE)),
            },
            local_server_url: std::env::var(NATS_URL_ENV).ok(),
        },
        HubTiming::default(),
    );
    area.spawn();
    area
}

fn unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}
