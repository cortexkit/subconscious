use std::{collections::BTreeMap, sync::atomic::Ordering, time::Duration};

use async_nats::{
    jetstream::{self, Message},
    HeaderMap,
};
use cortexkit_bus_naming::federation::HUB_API_PREFIX;
use tokio::sync::{watch, Mutex};

use super::{store::Store, Health};

const BATCH_WINDOW: Duration = Duration::from_secs(1);

pub(super) struct Purger {
    cuts: BTreeMap<String, watch::Sender<u64>>,
}

impl Purger {
    pub(super) fn new(streams: Vec<String>, journal: &Store) -> rusqlite::Result<Self> {
        let mut cuts = BTreeMap::new();
        for stream in streams {
            let cut = journal.purge_cut(&stream)?;
            cuts.insert(stream, watch::channel(cut).0);
        }
        Ok(Self { cuts })
    }

    pub(super) async fn confirmed(
        &self,
        journal: &Mutex<Store>,
        message: &Message,
        local_sequence: u64,
        verified: bool,
    ) -> rusqlite::Result<()> {
        let Some((source, sequence)) = source_sequence(message.headers.as_ref()) else {
            return Ok(());
        };
        let verified_cut = if verified && self.cuts.contains_key(source) {
            sequence.checked_add(1)
        } else {
            None
        };
        for (stream, cut) in journal.lock().await.confirm_purge_receipt(
            local_sequence,
            source,
            verified_cut,
            |name| self.cuts.contains_key(name),
        )? {
            self.cuts[&stream].send_if_modified(|prior| {
                if cut <= *prior {
                    return false;
                }
                *prior = cut;
                true
            });
        }
        Ok(())
    }

    pub(super) async fn run(&self, local: &jetstream::Context, health: &Health) {
        let hub = jetstream::with_prefix(local.client(), HUB_API_PREFIX);
        futures_util::future::join_all(
            self.cuts
                .iter()
                .map(|(stream, cut)| run_stream(&hub, health, stream, cut.subscribe())),
        )
        .await;
    }
}

async fn run_stream(
    hub: &jetstream::Context,
    health: &Health,
    source: &str,
    mut cuts: watch::Receiver<u64>,
) {
    cuts.mark_changed();
    while cuts.changed().await.is_ok() {
        // Coalesce a burst of acks. A failed cut is not retried unless a
        // later confirmation advances it (or the mover restarts).
        tokio::time::sleep(BATCH_WINDOW).await;
        let cut = *cuts.borrow_and_update();
        if cut == 0 {
            continue;
        }
        let result: Result<(), String> = async {
            let stream = hub
                .get_stream_no_info(source)
                .await
                .map_err(|error| error.to_string())?;
            let reply = stream
                .purge()
                .sequence(cut)
                .await
                .map_err(|error| error.to_string())?;
            reply
                .success
                .then_some(())
                .ok_or_else(|| "purge refused".into())
        }
        .await;
        if result.is_err() {
            health.hub_purge_failures.fetch_add(1, Ordering::Relaxed);
        }
        health.state(
            "fed.hub_purge_waiting",
            result.as_ref().err().map(String::as_str),
        );
    }
}

fn source_sequence(headers: Option<&HeaderMap>) -> Option<(&str, u64)> {
    let mut values = headers?.get_all("Nats-Stream-Source");
    let header = values.next()?.as_str();
    if values.next().is_some() {
        return None;
    }
    let mut fields = header.split_ascii_whitespace();
    // NATS appends an external API fingerprint to the source name. It is not
    // a hub identity: both hubs expose the same API prefix.
    let source = fields.next()?.split(':').next()?;
    let sequence = fields.next()?.parse::<u64>().ok()?;
    (sequence > 0).then_some((source, sequence))
}
