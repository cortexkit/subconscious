use std::{sync::atomic::Ordering, time::Duration};

use async_nats::{
    jetstream::{self, Message},
    HeaderMap,
};
use cortexkit_bus_naming::federation::{hub_inbox_stream_name, BoxId, HUB_API_PREFIX};
use tokio::sync::{watch, Mutex};

use super::{store::Store, Health};

// Production has no applied-hub fence yet. Do not grant or send a purge there.
pub(crate) const ENABLE_PRODUCTION_PURGE: bool = false;
const BATCH_WINDOW: Duration = Duration::from_secs(1);

/// The caller must detach the old source, read the last local sequence, and keep
/// the pinned hub route fixed while this generation's mover can issue purges.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Generation {
    pub(crate) pin: [u8; 32],
    pub(crate) floor: u64,
}

pub(super) struct Purger {
    generation: Generation,
    stream: String,
    cut: watch::Sender<u64>,
}

impl Purger {
    pub(super) fn new(
        generation: Generation,
        machine: &BoxId,
        journal: &mut Store,
    ) -> rusqlite::Result<Self> {
        let cut = journal.set_purge_generation(generation)?;
        Ok(Self {
            generation,
            stream: hub_inbox_stream_name(machine),
            cut: watch::channel(cut).0,
        })
    }

    pub(super) async fn confirmed(
        &self,
        journal: &Mutex<Store>,
        message: &Message,
        local_sequence: u64,
        verified: bool,
    ) -> rusqlite::Result<()> {
        if local_sequence <= self.generation.floor {
            return Ok(());
        }
        let Some(hub_sequence) = source_sequence(&self.stream, message.headers.as_ref()) else {
            return Ok(());
        };
        if let Some(cut) = journal.lock().await.confirm_purge_receipt(
            self.generation,
            local_sequence,
            hub_sequence,
            verified,
        )? {
            self.cut.send_if_modified(|prior| {
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
        if !ENABLE_PRODUCTION_PURGE && !cfg!(test) {
            return;
        }
        let hub = jetstream::with_prefix(local.client(), HUB_API_PREFIX);
        let mut cuts = self.cut.subscribe();
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
                    .get_stream_no_info(&self.stream)
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
}

fn source_sequence(stream: &str, headers: Option<&HeaderMap>) -> Option<u64> {
    let mut values = headers?.get_all("Nats-Stream-Source");
    let header = values.next()?.as_str();
    if values.next().is_some() {
        return None;
    }
    let mut fields = header.split_ascii_whitespace();
    // NATS appends an external API fingerprint to the source name. It is not
    // a hub identity: both hubs expose the same API prefix.
    if fields.next()?.split(':').next()? != stream {
        return None;
    }
    let sequence = fields.next()?.parse::<u64>().ok()?;
    (sequence > 0).then_some(sequence)
}
