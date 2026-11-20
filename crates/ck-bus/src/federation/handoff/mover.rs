use std::{
    collections::HashSet,
    error::Error as _,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use async_nats::{
    header::NATS_MESSAGE_ID,
    jetstream::{
        self,
        consumer::{pull, AckPolicy},
        message::PublishMessage,
        AckKind, Message,
    },
};
use cortexkit_bus_naming::{
    federation::{hub_inbox_stream_name, inbox_stream_name, BoxId, FEDOUT_STREAM},
    AccountNames,
};
use futures_util::{future::join_all, stream::FuturesUnordered, StreamExt as _};
use serde_json::json;
use tokio::sync::{Mutex, Notify};

use super::{
    in_durable, out_durable,
    purge::{Generation, Purger},
    store::{unix_secs, Source, Store},
    wire::{self, Envelope, Frame, Refusal, Stamp},
    Address, Health, OpenError, Opener, Peer, Peers, RECEIVE_FAMILIES, SEND_DURABLE,
};
use crate::{
    credentials::{roots::RootCredential, vault::VaultSigning},
    federation::leaf::area::log_event,
};

const ACK_WAIT: Duration = Duration::from_secs(10);
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(300);
const MAX_AGE_SECS: i64 = 7 * 24 * 60 * 60;
const HUB_FULL: jetstream::ErrorCode = jetstream::ErrorCode::STREAM_STORE_FAILED;

pub(crate) struct Mover {
    pub(crate) machine: BoxId,
    pub(crate) names: AccountNames,
    pub(crate) box_js: jetstream::Context,
    pub(crate) fed_js: jetstream::Context,
    pub(crate) peers: Arc<dyn Peers>,
    pub(crate) signing: Arc<dyn VaultSigning>,
    pub(crate) opener: Arc<dyn Opener>,
    pub(crate) health: Arc<Health>,
    journal: Mutex<Store>,
    recipient_changed: Notify,
    receiving: PendingReceipts,
    purger: Option<Purger>,
    open_rate_limit_delay: Duration,
}

// Keep replaceable I/O seams together, while the journal has one owner across the
// three directions. No network operation is performed with the journal locked.
pub(crate) struct Inputs {
    pub(crate) machine: BoxId,
    pub(crate) names: AccountNames,
    pub(crate) box_js: jetstream::Context,
    pub(crate) fed_js: jetstream::Context,
    pub(crate) peers: Arc<dyn Peers>,
    pub(crate) signing: Arc<dyn VaultSigning>,
    pub(crate) opener: Arc<dyn Opener>,
    pub(crate) health: Arc<Health>,
    pub(crate) purge_generation: Option<Generation>,
}

impl Mover {
    pub(crate) fn new(input: Inputs, path: &std::path::Path) -> rusqlite::Result<Self> {
        let mut journal = Store::open(path)?;
        let purger = input
            .purge_generation
            .map(|generation| Purger::new(generation, &input.machine, &mut journal))
            .transpose()?;
        Ok(Self {
            machine: input.machine,
            names: input.names,
            box_js: input.box_js,
            fed_js: input.fed_js,
            peers: input.peers,
            signing: input.signing,
            opener: input.opener,
            health: input.health,
            journal: Mutex::new(journal),
            recipient_changed: Notify::new(),
            receiving: std::sync::Mutex::new(HashSet::new()),
            purger,
            open_rate_limit_delay: Duration::from_secs(60),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_test_rate_limit_delay(mut self, delay: Duration) -> Self {
        self.open_rate_limit_delay = delay;
        self
    }

    pub(crate) async fn run(&self) {
        tokio::join!(
            self.run_lane(Lane::Send),
            self.run_forwarders(),
            join_all(RECEIVE_FAMILIES.map(|family| self.run_lane(Lane::Receive(family)))),
            self.run_pruner(),
            async {
                if let Some(purger) = &self.purger {
                    purger.run(&self.fed_js, &self.health).await;
                }
            },
        );
    }

    async fn run_pruner(&self) {
        let mut ticks = tokio::time::interval(Duration::from_secs(60 * 60));
        loop {
            ticks.tick().await;
            let result = self.journal.lock().await.prune(unix_secs());
            let error = result.err().map(|error| error.to_string());
            self.health
                .state("fed.dedupe_prune_waiting", error.as_deref());
        }
    }

    async fn run_forwarders(&self) {
        let mut started = HashSet::new();
        let mut lanes = FuturesUnordered::new();
        loop {
            let peers = self.journal.lock().await.recipients();
            match peers {
                Ok(peers) => {
                    self.health.state("fed.outbox_scan_waiting", None);
                    for peer in peers {
                        if started.insert(peer.clone()) {
                            lanes.push(self.run_lane(Lane::Forward(peer)));
                        }
                    }
                }
                Err(error) => self
                    .health
                    .state("fed.outbox_scan_waiting", Some(&error.to_string())),
            }
            // The recipient list is recorded before the first FEDOUT publish. Scanning
            // it at startup also restarts readers for frames already stored by NATS.
            tokio::select! {
                () = self.recipient_changed.notified() => {},
                _ = lanes.next(), if !lanes.is_empty() => {},
                () = tokio::time::sleep(Duration::from_secs(5)) => {},
            }
        }
    }

    async fn run_lane(&self, lane: Lane) {
        let (js, stream, durable, filters) = match &lane {
            Lane::Send => (
                &self.box_js,
                self.names.streams().fedsend.clone(),
                SEND_DURABLE.to_owned(),
                vec![],
            ),
            Lane::Forward(peer) => (
                &self.fed_js,
                FEDOUT_STREAM.into(),
                out_durable(peer),
                vec![format!("fedout.{peer}.>")],
            ),
            Lane::Receive(family) => (
                &self.fed_js,
                inbox_stream_name(&self.machine),
                in_durable(family),
                vec![format!("ck.{}.{family}.>", self.machine)],
            ),
        };
        loop {
            let error = self.pull(js, &stream, &durable, &filters, &lane).await;
            self.health.state(&lane.health(), Some(&error));
            tokio::time::sleep(FIRST_BACKOFF).await;
        }
    }

    async fn pull(
        &self,
        js: &jetstream::Context,
        stream: &str,
        durable: &str,
        filters: &[String],
        lane: &Lane,
    ) -> String {
        let config = pull::Config {
            durable_name: Some(durable.to_string()),
            ack_policy: AckPolicy::Explicit,
            ack_wait: ACK_WAIT,
            max_deliver: -1,
            // A forwarder holds its oldest item until a hub PubAck. Other lanes may
            // delay an individual delivery with a NAK and pull the next available one.
            max_ack_pending: if matches!(lane, Lane::Forward(_)) {
                1
            } else {
                -1
            },
            filter_subjects: filters.to_vec(),
            ..Default::default()
        };
        let consumer = match js.create_consumer_on_stream(config, stream).await {
            Ok(consumer) => consumer,
            Err(error) => return format!("open {stream}/{durable}: {error}"),
        };
        if matches!(lane, Lane::Send) && consumer.cached_info().num_ack_pending > 0 {
            // On restart, let a sealed but unsettled frame become due before
            // pulling any new frames that could take its place in FEDOUT.
            tokio::time::sleep(ACK_WAIT + FIRST_BACKOFF).await;
        }
        self.health.state(&lane.health(), None);
        loop {
            // Fetch a single message so no later delivery can be buffered ahead of
            // the current frame, even if publishing crosses an outage.
            let mut batch = match consumer
                .fetch()
                .max_messages(1)
                .expires(Duration::from_secs(5))
                .messages()
                .await
            {
                Ok(batch) => batch,
                Err(error) => return format!("pull {stream}/{durable}: {error}"),
            };
            match batch.next().await {
                Some(Ok(message)) => {
                    tokio::select! {
                        () = self.process(lane, &message) => {},
                        () = keep_pending(&message) => {},
                    }
                }
                Some(Err(error)) => return format!("read {stream}/{durable}: {error}"),
                None => {}
            }
        }
    }

    async fn process(&self, lane: &Lane, message: &Message) {
        let health = lane.health();
        let Ok(metadata) = message.info() else {
            self.health
                .state(&health, Some("delivery has no JetStream metadata"));
            return;
        };
        let expiry = metadata
            .published
            .unix_timestamp()
            .saturating_add(MAX_AGE_SECS);
        let redelivery_delay = (FIRST_BACKOFF
            * (1 << metadata.delivered.saturating_sub(1).clamp(0, 9) as u32))
            .min(MAX_BACKOFF);
        let source = Source::new(
            metadata.stream_sequence,
            &message.subject,
            message_id(message),
        );
        let mut backoff = FIRST_BACKOFF;
        // Once the next hop has stored this message, retry only the ack, not the
        // publish. After a crash the journal and the next hop's dedupe take over.
        let mut settling = None;
        loop {
            let result = if let Some(verified) = settling {
                Ok(verified)
            } else {
                match lane {
                    Lane::Send => self.send(message, &source).await.map(|()| false),
                    Lane::Forward(peer) => self.forward(message, peer).await.map(|()| false),
                    Lane::Receive(_) => self.receive(message, &source).await,
                }
            };
            let delay = match result {
                Ok(verified) => {
                    settling = Some(verified);
                    match message.double_ack().await {
                        Ok(()) => {
                            if matches!(lane, Lane::Send) {
                                if let Err(error) = self.journal.lock().await.forget_seal(&source) {
                                    self.health.state(&health, Some(&error.to_string()));
                                    return;
                                }
                            }
                            if matches!(lane, Lane::Receive(_)) {
                                self.confirm_inbox_ack(message, source.sequence, verified)
                                    .await;
                            }
                            self.health.state(&health, None);
                            return;
                        }
                        Err(error) => {
                            self.health
                                .state(&health, Some(&format!("acknowledgement: {error}")));
                            backoff
                        }
                    }
                }
                Err(error) => {
                    self.health.state(&health, Some(&error.detail));
                    if error.defer {
                        if matches!(lane, Lane::Receive(_)) {
                            self.health.inbound_waits.fetch_add(1, Ordering::Relaxed);
                        }
                        let delay = redelivery_delay.max(error.minimum_delay);
                        // Delay just this record. A later frame may be verifiable even
                        // when this one's roster record is stale or ambiguous.
                        if let Err(error) = message.ack_with(AckKind::Nak(Some(delay))).await {
                            self.health
                                .state(&health, Some(&format!("delay delivery: {error}")));
                        }
                        return;
                    }
                    backoff
                }
            };
            let remaining = expiry.saturating_sub(unix_secs());
            if remaining <= 0 {
                return;
            }
            tokio::time::sleep(delay.min(Duration::from_secs(remaining as u64))).await;
            if expiry <= unix_secs() {
                return;
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    async fn confirm_inbox_ack(&self, message: &Message, sequence: u64, verified: bool) {
        if let Some(purger) = &self.purger {
            let result = purger
                .confirmed(&self.journal, message, sequence, verified)
                .await;
            let error = result.err().map(|error| error.to_string());
            self.health
                .state("fed.hub_purge_journal_waiting", error.as_deref());
        }
    }

    async fn send(&self, message: &Message, source: &Source) -> Result<(), Retry> {
        let prefix = format!("ck.{}.fedsend.", self.names.account());
        let subject = message.subject.strip_prefix(&prefix);
        let result = match subject.and_then(|suffix| suffix.split_once('.')) {
            Some((peer, tail)) => self.prepare(message, source, peer, tail).await?,
            None => Err(Refusal::Subject),
        };
        match result {
            Ok((subject, id, blob)) => {
                publish(&self.fed_js, &subject, &id, blob, FEDOUT_STREAM).await?;
            }
            Err(reason) => {
                let source = source.sequence;
                let detail = reason
                    .detail()
                    .map_or_else(String::new, |hop| format!(".{hop}"));
                let dead = format!(
                    "ck.{}.fedsend_dead.{}{detail}.{source}",
                    self.names.account(),
                    reason.code()
                );
                let id = message_id(message)
                    .filter(|id| wire::check_fields("", id).is_ok())
                    .unwrap_or_default();
                publish(
                    &self.box_js,
                    &dead,
                    id,
                    message.payload.to_vec(),
                    &self.names.streams().fedsend_dead,
                )
                .await?;
                self.health.dead.fetch_add(1, Ordering::Relaxed);
                log_event(
                    "ckbus.federation.fedsend_dead",
                    json!({
                        "reason": reason.code(), "limit": reason.detail(), "source_sequence": source,
                    }),
                );
            }
        }
        Ok(())
    }

    async fn prepare(
        &self,
        message: &Message,
        source: &Source,
        peer: &str,
        tail: &str,
    ) -> Result<Result<(String, String, Vec<u8>), Refusal>, Retry> {
        let Ok(peer) = BoxId::parse(peer) else {
            return Ok(Err(Refusal::Subject));
        };
        let Ok(remote_names) = AccountNames::derive(&format!("box_{peer}")) else {
            return Ok(Err(Refusal::Subject));
        };
        let id = message_id(message).unwrap_or_default();
        if let Err(error) = Address::parse(tail, &remote_names, &self.machine)
            .and_then(|_| wire::check_fields(tail, id))
            .and_then(|_| wire::check_size(tail, id, &message.payload))
        {
            return Ok(Err(error));
        }
        let subject = format!("fedout.{peer}.{tail}");
        let previous = self.journal.lock().await.sealed(source)?;
        if let Some(blob) = previous {
            return Ok(Ok((subject, id.to_owned(), blob)));
        }
        let key = match self.peers.lookup(&peer).await.map_err(Retry::new)? {
            Peer::Unpaired => return Ok(Err(Refusal::Unpaired)),
            Peer::MissingKey => return Ok(Err(Refusal::NoKey)),
            Peer::Ambiguous => return Err(self.ambiguous_peer()),
            Peer::Live(keys) => keys.seal,
        };
        let stamp = {
            let journal = self.journal.lock().await;
            Stamp {
                epoch: journal.epoch(),
                sequence: journal.next(&peer)?,
            }
        };
        let transcript = wire::transcript(&self.machine, &peer, tail, id, stamp, &message.payload);
        let credential = RootCredential::MessageSigning
            .credential_id()
            .map_err(|error| Retry::new(error.to_string()))?;
        let signature = self
            .signing
            .sign(&credential, &transcript)
            .await
            .map_err(|error| Retry::new(error.to_string()))?;
        let blob = match wire::seal(&self.machine, &peer, &key, transcript, &signature.signature) {
            Ok(blob) => blob,
            Err(_) => return Ok(Err(Refusal::NoKey)),
        };
        self.journal
            .lock()
            .await
            .save_seal(source, &peer, stamp.sequence, &blob)?;
        self.recipient_changed.notify_one();
        Ok(Ok((subject, id.to_owned(), blob)))
    }

    async fn forward(&self, message: &Message, peer: &BoxId) -> Result<(), Retry> {
        let tail = message
            .subject
            .strip_prefix(&format!("fedout.{peer}."))
            .ok_or_else(|| Retry::new("invalid FEDOUT subject"))?;
        let id = message_id(message).ok_or_else(|| Retry::new("FEDOUT has no Nats-Msg-Id"))?;
        let result = publish(
            &self.fed_js,
            &format!("ck.{peer}.{tail}"),
            id,
            message.payload.to_vec(),
            &hub_inbox_stream_name(peer),
        )
        .await;
        if let Err(error) = &result {
            if error.full {
                self.health.hub_full.fetch_add(1, Ordering::Relaxed);
            } else {
                self.health.hub_unavailable.fetch_add(1, Ordering::Relaxed);
            }
        }
        result
    }

    // Only an opened and verified frame may set the hub cut, including a duplicate.
    async fn receive(&self, message: &Message, source: &Source) -> Result<bool, Retry> {
        if self.journal.lock().await.quarantined(source)? {
            return Ok(false);
        }
        match self.open_frame(message).await? {
            Ok(opened) => {
                let Stamp { epoch, sequence } = opened.stamp;
                let _claim = ReceiptClaim::take(&self.receiving, &opened.sender, opened.stamp)
                    .ok_or_else(|| Retry::defer("duplicate delivery is already in progress"))?;
                if self
                    .journal
                    .lock()
                    .await
                    .seen(&opened.sender, epoch, sequence)?
                {
                    self.health.duplicate.fetch_add(1, Ordering::Relaxed);
                    return Ok(true);
                }
                publish(
                    &self.fed_js,
                    &opened.local,
                    opened.id,
                    opened.body,
                    opened.stream,
                )
                .await
                .map_err(|error| Retry::defer(error.detail))?;
                let gap = self
                    .journal
                    .lock()
                    .await
                    .received(&opened.sender, epoch, sequence)?;
                if let Some(expected) = gap {
                    self.health.gaps.fetch_add(1, Ordering::Relaxed);
                    log_event(
                        "ckbus.federation.sequence_gap",
                        json!({
                            "sender": opened.sender.as_str(), "epoch": epoch, "expected": expected, "received": sequence,
                        }),
                    );
                }
                Ok(true)
            }
            Err(reason) => {
                let origin = Envelope::claimed_sender(&message.payload);
                if self.journal.lock().await.quarantine(
                    source,
                    &message.subject,
                    origin.as_ref(),
                    &reason,
                    &message.payload,
                )? {
                    self.health.quarantined.fetch_add(1, Ordering::Relaxed);
                    log_event(
                        "ckbus.federation.quarantined",
                        json!({"reason": reason, "source_sequence": source.sequence}),
                    );
                }
                Ok(false)
            }
        }
    }

    async fn open_frame<'a>(
        &'a self,
        message: &'a Message,
    ) -> Result<Result<Opened<'a>, String>, Retry> {
        let envelope = match Envelope::parse(&message.payload) {
            Ok(envelope) => envelope,
            Err(reason) => return refused(reason),
        };
        let Some(tail) = message
            .subject
            .strip_prefix(&format!("ck.{}.", self.machine))
        else {
            return refused(Refusal::Subject);
        };
        let address = match Address::parse(tail, &self.names, &envelope.sender) {
            Ok(address) => address,
            Err(reason) => return refused(reason),
        };
        let id = message_id(message).unwrap_or_default();
        let info = wire::info(&envelope.sender, &self.machine);
        let plaintext = match self
            .opener
            .open(envelope.enc, envelope.ciphertext, info.as_bytes())
            .await
        {
            Ok(plaintext) => plaintext,
            Err(OpenError::Unavailable(error)) => return Err(Retry::defer(error)),
            Err(OpenError::RateLimited) => {
                return Err(Retry {
                    minimum_delay: self.open_rate_limit_delay,
                    ..Retry::defer("open_rate_limited")
                })
            }
            Err(OpenError::Refused(reason)) => return Ok(Err(reason)),
        };
        let frame = match Frame::check(&plaintext, &envelope.sender, &self.machine, tail, id) {
            Ok(frame) => frame,
            Err(reason) => return refused(reason),
        };
        let key = match self
            .peers
            .lookup(&envelope.sender)
            .await
            .map_err(Retry::defer)?
        {
            Peer::Unpaired => return Err(Retry::defer(Refusal::Unpaired.code())),
            Peer::MissingKey => return Err(Retry::defer(Refusal::NoKey.code())),
            Peer::Ambiguous => return Err(self.ambiguous_peer()),
            Peer::Live(key) => key.sign,
        };
        if let Err(reason) = frame.verify(&key) {
            return Err(Retry::defer(reason.code()));
        }
        Ok(Ok(Opened {
            local: address.local(&envelope.sender),
            sender: envelope.sender,
            stamp: frame.stamp,
            id,
            stream: address.stream,
            body: frame.body.to_vec(),
        }))
    }

    fn ambiguous_peer(&self) -> Retry {
        self.health.ambiguous_peers.fetch_add(1, Ordering::Relaxed);
        Retry::defer("multiple key records for one machine id; treating the peer as unknown")
    }
}

type PendingReceipts = std::sync::Mutex<HashSet<(String, Stamp)>>;

// A valid sender can reuse a sequence across families. Exactly one lane may try
// to store that identity, without holding the journal lock during the publish.
struct ReceiptClaim<'a> {
    pending: &'a PendingReceipts,
    key: (String, Stamp),
}

impl<'a> ReceiptClaim<'a> {
    fn take(pending: &'a PendingReceipts, sender: &BoxId, stamp: Stamp) -> Option<Self> {
        let key = (sender.as_str().to_owned(), stamp);
        pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(key.clone())
            .then(|| Self { pending, key })
    }
}

impl Drop for ReceiptClaim<'_> {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.key);
    }
}

fn refused<T>(reason: Refusal) -> Result<Result<T, String>, Retry> {
    Ok(Err(reason.code().to_owned()))
}

struct Opened<'a> {
    sender: BoxId,
    stamp: Stamp,
    local: String,
    id: &'a str,
    stream: &'a str,
    body: Vec<u8>,
}

enum Lane {
    Send,
    Forward(BoxId),
    Receive(&'static str),
}

impl Lane {
    fn health(&self) -> String {
        match self {
            Self::Send => "fed.fedsend_waiting".into(),
            Self::Forward(peer) => format!("fed.forward_waiting.{peer}"),
            Self::Receive(family) => format!("fed.receive_waiting.{family}"),
        }
    }
}

fn message_id(message: &Message) -> Option<&str> {
    let headers = message.headers.as_ref()?;
    let mut values = headers.get_all(NATS_MESSAGE_ID);
    let id = values.next()?.as_str();
    values.next().is_none().then_some(id)
}

struct Retry {
    detail: String,
    full: bool,
    defer: bool,
    minimum_delay: Duration,
}

impl Retry {
    fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            full: false,
            defer: false,
            minimum_delay: Duration::ZERO,
        }
    }

    fn defer(detail: impl Into<String>) -> Self {
        Self {
            defer: true,
            ..Self::new(detail)
        }
    }
}

async fn keep_pending(message: &Message) {
    loop {
        tokio::time::sleep(ACK_WAIT / 3).await;
        let _ = message.ack_with(AckKind::Progress).await;
    }
}

impl From<rusqlite::Error> for Retry {
    fn from(error: rusqlite::Error) -> Self {
        Retry::new(format!("hand-off journal: {error}"))
    }
}

async fn publish(
    js: &jetstream::Context,
    subject: &str,
    id: &str,
    body: Vec<u8>,
    stream: &str,
) -> Result<(), Retry> {
    let mut publish = PublishMessage::build().payload(body.into());
    if !id.is_empty() {
        publish = publish.message_id(id);
    }
    let ack = js
        .send_publish(subject.to_owned(), publish)
        .await
        .map_err(|error| Retry::new(error.to_string()))?;
    let ack = ack.await.map_err(|error| {
        let full = error
            .source()
            .and_then(|source| source.downcast_ref::<jetstream::Error>())
            .is_some_and(|api| api.error_code() == HUB_FULL);
        Retry {
            full,
            ..Retry::new(error.to_string())
        }
    })?;
    if ack.stream != stream {
        return Err(Retry::new(format!(
            "expected {stream} PubAck, got {}",
            ack.stream
        )));
    }
    Ok(())
}
