//! Production connections for the hand-off mover. The only login that imports
//! deliveries into the box belongs to the federation account, not to a box user.

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_nats::jetstream::{self, stream};
use async_trait::async_trait;
use cortexkit_bus_naming::{
    federation::{
        deliver_prefix, hub_inbox_stream_name, inbox_stream_name, BoxId, HubName, FEDOUT_STREAM,
        HUB_API_PREFIX,
    },
    AccountNames,
};
use serde_json::Value;
use tokio::sync::{broadcast, watch};

use super::{
    in_durable,
    mover::{Inputs, Mover},
    store::Store as Journal,
    Health, OpenError, Opener, Peer, PeerKeys, Peers, RECEIVE_FAMILIES,
};
use crate::{
    bootstrap::{
        account_jwt::HANDOFF_FAMILIES, config::BrokerConfig, federation_account, plane::NatsBroker,
        store::Store, Ready,
    },
    credentials::{
        issue::{sign_user_jwt, UserJwtRequest},
        renewal::{self, OwnRenewal, RenewalTask},
        roots::RootCredential,
        vault::{ClaustrumRoute, VaultError},
        wire::decode_hex_lower,
        Credentials,
    },
    federation::{
        hub::{devices::parse_devices, seams::PairedDevices},
        leaf::{area::InboxRoute, production::SubcFederation},
    },
    grants::{Grant, GrantRole},
};

const RETRY: Duration = Duration::from_secs(5);
const USER_NAME: &str = "ckbus-handoff";

pub(crate) fn start(
    callosum: Arc<SubcFederation>,
    opener: Arc<ClaustrumRoute>,
    credentials: Arc<Credentials>,
    root: PathBuf,
    mut ready: watch::Receiver<Option<Arc<Ready>>>,
    mut route: watch::Receiver<Option<InboxRoute>>,
) -> Arc<Health> {
    let health = Arc::new(Health::default());
    let report = health.clone();
    tokio::spawn(async move {
        let plane = match ready.wait_for(Option::is_some).await {
            Ok(ready) => ready.as_ref().cloned().expect("waited for bootstrap"),
            Err(_) => return,
        };
        let store = Store::new(root);
        loop {
            let pinned = route.borrow_and_update().clone();
            match connect(&credentials, &store, &plane, pinned.as_ref()).await {
                Ok(connection) => {
                    let state_path = journal_path(&store, &connection.machine);
                    let Some(box_js) = plane.box_plane.handoff_jetstream() else {
                        report.state("fed.mover_waiting", Some("no box connection for the mover"));
                        return;
                    };
                    let mover = Mover::new(
                        Inputs {
                            machine: connection.machine.clone(),
                            names: connection.names.clone(),
                            box_js,
                            fed_js: connection.js.clone(),
                            peers: callosum.clone(),
                            signing: credentials.vault.clone(),
                            opener: opener.clone(),
                            health: report.clone(),
                            hub: pinned.map(|route| route.hub),
                            sources: connection.sources.clone(),
                            inbox_floor: connection.inbox_floor,
                        },
                        &state_path,
                    );
                    match mover {
                        Ok(mover) => {
                            report.state("fed.mover_waiting", None);
                            tokio::select! {
                                () = mover.run() => return,
                                changed = route.changed() => if changed.is_err() { return; },
                            }
                            continue;
                        }
                        Err(error) => report.state(
                            "fed.mover_waiting",
                            Some(&format!("open hand-off journal: {error}")),
                        ),
                    }
                }
                Err(error) => report.state("fed.mover_waiting", Some(&error)),
            }
            tokio::time::sleep(RETRY).await;
        }
    });
    health
}

struct Connection {
    machine: BoxId,
    names: AccountNames,
    js: jetstream::Context,
    sources: Vec<String>,
    inbox_floor: u64,
    _renewal: RenewalTask,
    credentials: Arc<Credentials>,
    user: String,
}

fn journal_path(store: &Store, machine: &BoxId) -> PathBuf {
    store.root().join(format!("handoff-v1-{machine}.sqlite3"))
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.credentials.custody.forget(&self.user);
    }
}

async fn connect(
    credentials: &Arc<Credentials>,
    store: &Store,
    ready: &Ready,
    route: Option<&InboxRoute>,
) -> Result<Connection, String> {
    let machine = BoxId::parse(&ready.account.machine_id).map_err(|error| error.to_string())?;
    let names = AccountNames::derive(&ready.account.acct).map_err(|error| error.to_string())?;
    let broker = NatsBroker::new(BrokerConfig::from_env()?.url, credentials.clone());
    let issuer = ready
        .fedout_account
        .get_or_try_init(|| async {
            federation_account::provision_fedout(
                ready.system.as_ref(),
                &broker,
                credentials,
                store,
                machine.as_str(),
            )
            .await?
            .ok_or_else(|| "FEDOUT is waiting for the federation signing key".to_owned())
        })
        .await?;
    let root = RootCredential::FederationAccount
        .credential_id()
        .map_err(|error| error.to_string())?;
    let issued_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let user = credentials.custody.generate_user();
    let sources = route.map_or_else(Vec::new, |route| inbox_sources(&machine, route));
    let grant = mover_grant(&machine, &user, &sources);
    let result = async {
        let signed = sign_user_jwt(
            credentials.vault.as_ref(),
            &credentials.key_ids,
            &UserJwtRequest {
                root_credential_id: &root,
                user_public: &user,
                issuer_account: Some(issuer),
                name: USER_NAME,
                issued_at,
                expires_at: credentials.lifetime.expires_at(issued_at),
                grant: &grant,
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        let (client, _) = broker
            .connect(&signed.jwt, &user, USER_NAME, broadcast::channel(1).0)
            .await
            .map_err(|error| error.to_string())?;
        let js = jetstream::new(client);
        let inbox_floor = ensure_inbox(&js, &machine, &sources, |floor| {
            Journal::open(&journal_path(store, &machine))
                .and_then(|journal| journal.initialize_inbox_prefix(floor))
                .map_err(|error| format!("initialize hand-off journal: {error}"))
        })
        .await?;
        let renewal = renewal::spawn(
            credentials.clone(),
            OwnRenewal {
                root_credential_id: root,
                user_public: user.clone(),
                issuer_account: issuer.clone(),
                name: USER_NAME.to_owned(),
                grant,
            },
            issued_at,
        );
        Ok(Connection {
            machine,
            names,
            js,
            sources,
            inbox_floor,
            _renewal: renewal,
            credentials: credentials.clone(),
            user: user.clone(),
        })
    }
    .await;
    if result.is_err() {
        credentials.custody.forget(&user);
    }
    result
}

fn inbox_sources(machine: &BoxId, route: &InboxRoute) -> Vec<String> {
    let current = hub_inbox_stream_name(machine, &route.hub);
    // The JWT can also grant a draining inbox after a key rotation. Accept those
    // extra names only if it also grants the inbox named by the current hub key.
    if !route.purge_streams.contains(&current) {
        return Vec::new();
    }
    let prefix = format!("HUB_INBOX_{machine}_");
    route
        .purge_streams
        .iter()
        .filter(|stream| {
            stream
                .strip_prefix(&prefix)
                .is_some_and(|suffix| HubName::parse(suffix).is_ok())
        })
        .cloned()
        .collect()
}

pub(crate) fn mover_grant(machine: &BoxId, user: &str, sources: &[String]) -> Grant {
    let inbox = inbox_stream_name(machine);
    let mut publish = vec![
        format!("$JS.API.STREAM.INFO.{inbox}"),
        format!("$JS.API.STREAM.CREATE.{inbox}"),
        format!("$JS.API.STREAM.UPDATE.{inbox}"),
        "fedout.>".into(),
    ];
    for source in sources {
        publish.push(format!("{HUB_API_PREFIX}.STREAM.PURGE.{source}"));
    }
    // A recipient is learned after the mover connects. Limit the wildcard to the
    // consumer token on FEDOUT; each receive consumer has an exact grant.
    let receive = RECEIVE_FAMILIES.map(in_durable);
    let consumers = std::iter::once((FEDOUT_STREAM, "*")).chain(
        receive
            .iter()
            .map(|durable| (inbox.as_str(), durable.as_str())),
    );
    for (stream, durable) in consumers {
        publish.extend([
            format!("$JS.API.CONSUMER.CREATE.{stream}.{durable}"),
            format!("$JS.API.CONSUMER.DURABLE.CREATE.{stream}.{durable}"),
            format!("$JS.API.CONSUMER.INFO.{stream}.{durable}"),
            format!("$JS.API.CONSUMER.MSG.NEXT.{stream}.{durable}"),
            format!("$JS.ACK.{stream}.{durable}.>"),
        ]);
    }
    for family in HANDOFF_FAMILIES {
        publish.extend([
            format!("fedlocal.{family}.fed.>"),
            format!("ck.*.{family}.>"),
        ]);
    }
    Grant::with_denies(
        GrantRole::BusModule,
        &federation_account::federation_account_name(machine.as_str()),
        publish.into_iter().collect(),
        [format!("_INBOX.{user}.>")].into(),
        Vec::new(),
    )
}

pub(crate) async fn ensure_inbox(
    js: &jetstream::Context,
    machine: &BoxId,
    sources: &[String],
    initialize_prefix: impl FnOnce(u64) -> Result<(), String>,
) -> Result<u64, String> {
    let config = stream::Config {
        name: inbox_stream_name(machine),
        subjects: if sources.is_empty() {
            vec![inbox_stream_name(machine)]
        } else {
            Vec::new()
        },
        sources: (!sources.is_empty()).then(|| {
            sources
                .iter()
                .map(|name| stream::Source {
                    name: name.clone(),
                    external: Some(stream::External {
                        api_prefix: HUB_API_PREFIX.to_owned(),
                        delivery_prefix: Some(deliver_prefix(machine)),
                    }),
                    ..Default::default()
                })
                .collect()
        }),
        storage: stream::StorageType::File,
        discard: stream::DiscardPolicy::New,
        max_age: Duration::from_secs(7 * 24 * 60 * 60),
        ..Default::default()
    };
    let inbox = js
        .get_or_create_stream(config.clone())
        .await
        .map_err(|error| error.to_string())?;
    let info = inbox.cached_info();
    let actual = &info.config;
    if !(actual.subjects.is_empty() || actual.subjects == [config.name.as_str()])
        || actual.mirror.is_some()
        || actual.storage != config.storage
        || actual.discard != config.discard
        || actual.max_age != config.max_age
    {
        return Err(format!(
            "{} has a different inbox configuration; not replacing it",
            config.name
        ));
    }
    let legacy = format!("HUB_INBOX_{machine}");
    // Existing legacy deliveries still open normally, but none may set the first
    // named purge cut. An already-ACKed legacy frame won't be delivered again.
    let floor = if actual
        .sources
        .iter()
        .flatten()
        .any(|source| source.name == legacy)
    {
        info.state.last_sequence
    } else {
        info.state.first_sequence.saturating_sub(1)
    };
    initialize_prefix(floor)?;
    if actual.sources != config.sources || actual.subjects != config.subjects {
        let mut update = actual.clone();
        update.sources = config.sources;
        update.subjects = config.subjects;
        js.update_stream(update)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(floor)
}

#[async_trait]
impl Opener for ClaustrumRoute {
    async fn open(&self, enc: &[u8], ciphertext: &[u8], info: &[u8]) -> Result<Vec<u8>, OpenError> {
        match self.open_bus_seal(enc, ciphertext, info).await {
            Ok(plaintext) => Ok(plaintext),
            Err(error) => Err(classify_open(error)),
        }
    }
}

fn classify_open(error: VaultError) -> OpenError {
    match error {
        VaultError::Refused { ref code, .. }
        | VaultError::Route {
            code: Some(ref code),
            ..
        } if code == "open_rate_limited" => OpenError::RateLimited,
        VaultError::Refused { ref class, .. } if class.as_deref() == Some("transient") => {
            OpenError::Unavailable(error.to_string())
        }
        VaultError::Refused { code, .. } if code == "open_failed" => OpenError::Refused(code),
        VaultError::RootKeyUnreachable { .. } => OpenError::Unavailable("not_found".into()),
        // A bad reply, bad grant or protocol error cannot prove the frame is bad.
        error => OpenError::Unavailable(error.to_string()),
    }
}

#[async_trait]
impl Peers for SubcFederation {
    async fn lookup(&self, peer: &BoxId) -> Result<Peer, String> {
        let document: Value = serde_json::from_slice(&PairedDevices::peer_keys(self).await?)
            .map_err(|error| error.to_string())?;
        let records = document["peers"]
            .as_array()
            .ok_or("the peer keys answer has no peers list")?;
        let mut records = records
            .iter()
            .filter(|record| record["machine_id"].as_str() == Some(peer.as_str()));
        let Some(record) = records.next() else {
            return Ok(Peer::Unpaired);
        };
        if records.next().is_some() {
            return Ok(Peer::Ambiguous);
        }
        let fingerprint = record["fingerprint"]
            .as_str()
            .ok_or("peer key record has no fingerprint")?;
        if !parse_devices(&PairedDevices::read(self).await?)?.is_live_computer(fingerprint) {
            return Ok(Peer::Unpaired);
        }
        let keys = &record["key_record"]["keys"];
        let public = |name: &str| -> Option<[u8; 32]> {
            decode_hex_lower(keys[name]["pub"].as_str()?)?
                .try_into()
                .ok()
        };
        Ok(
            match (public("bus_seal_x25519"), public("msgsig_ed25519")) {
                (Some(seal), Some(sign)) => Peer::Live(PeerKeys { seal, sign }),
                _ => Peer::MissingKey,
            },
        )
    }
}
