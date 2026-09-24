//! `c_ckbus_dead` on the server: set up at start (floor read, delete, create), then
//! one record at a time: read, record, settle.

use std::{sync::Arc, time::Duration};

use async_nats::jetstream::{
    self,
    consumer::{pull, AckPolicy, DeliverPolicy, PullConsumer},
    stream::ConsumerErrorKind,
    ErrorCode,
};
use cortexkit_bus_naming::{shipped_streams, validate_consumer, AccountNames, ConsumerSpec};
use futures_util::StreamExt;
use serde_json::json;
use tokio::sync::watch;

use super::{
    event, parse_record, Classification, Journal, Ledger, StderrJournal, ACK_WAIT, CONSUMER_ID,
    MAX_ACK_PENDING, MAX_DELIVER, MAX_WAITING, PULL_EXPIRES,
};
use crate::{bootstrap::Ready, grants};

/// Causes named in `ckbus.dead_letter.down`.
pub mod cause {
    pub const NAMING_CONSTRUCTOR_ABSENT: &str = "naming-constructor-absent";
    pub const CONSUMER_REFUSED: &str = "dead-letter-consumer-refused";
    pub const CONSUMER_UNAVAILABLE: &str = "dead-letter-consumer-unavailable";
    pub const LINK_ABSENT: &str = "dead-letter-link-absent";
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Down {
    pub cause: &'static str,
    pub message: String,
}

impl Down {
    fn new(cause: &'static str, message: impl Into<String>) -> Self {
        Self {
            cause,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Down {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.cause, self.message)
    }
}

/// The places a test can stop the consumer, exactly as a process death there would:
/// after a record is read and before its line is written, and after the line and
/// before the record is settled. Outside tests nothing stops at them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boundary {
    Read,
    Recorded,
}

/// `c_ckbus_dead`'s configuration: the literal dead-letter subject as its only filter,
/// replay from the first retained record, and every limit stated.
pub fn consumer_config(names: &AccountNames) -> Result<(String, pull::Config), Down> {
    let durable = AccountNames::consumer_name(CONSUMER_ID)
        .map_err(|error| Down::new(cause::NAMING_CONSTRUCTOR_ABSENT, error.to_string()))?;
    let stream = names.streams().effect_dead.clone();
    let spec = ConsumerSpec {
        durable: durable.clone(),
        stream: stream.clone(),
        filter_subjects: vec![names.effect_dead()],
    };
    // The filter is checked against the stream's binding before the server is asked.
    validate_consumer(&spec, &shipped_streams(names))
        .map_err(|error| Down::new(cause::CONSUMER_REFUSED, error.to_string()))?;
    Ok((
        stream,
        pull::Config {
            durable_name: Some(durable),
            deliver_policy: DeliverPolicy::All,
            ack_policy: AckPolicy::Explicit,
            ack_wait: ACK_WAIT,
            max_deliver: MAX_DELIVER,
            max_ack_pending: MAX_ACK_PENDING,
            max_waiting: MAX_WAITING,
            filter_subjects: spec.filter_subjects,
            ..Default::default()
        },
    ))
}

pub struct DeadLetter {
    consumer: PullConsumer,
    journal: Arc<dyn Journal>,
    ledger: Ledger,
    /// Records at or below this stream sequence were settled by an earlier process, so
    /// they were already recorded: they rebuild the ledger and write no line.
    settled_floor: u64,
    #[cfg(test)]
    pub stop_at: Option<Boundary>,
}

impl DeadLetter {
    /// Sets `c_ckbus_dead` up for this process: reads the previous consumer's ack floor,
    /// deletes it and creates it again, so the stream is replayed from its first
    /// retained record. A consumer that is not there has a floor of 0. Any other failed
    /// read stops here: a failed read is never taken for an absent consumer, since that
    /// would record every retained record again.
    pub async fn open(
        client: async_nats::Client,
        names: &AccountNames,
        journal: Arc<dyn Journal>,
    ) -> Result<Self, Down> {
        let (stream, config) = consumer_config(names)?;
        let durable = config.durable_name.clone().unwrap_or_default();
        let js = jetstream::new(client);
        let previous_floor = match js
            .get_consumer_from_stream::<pull::Config, _, _>(&durable, &stream)
            .await
        {
            Ok(previous) => Some(previous.cached_info().ack_floor.stream_sequence),
            Err(error) if not_found(error.kind()) => None,
            Err(error) => return Err(unavailable(&durable, "info", error)),
        };
        let settled_floor = previous_floor.unwrap_or_default();
        if previous_floor.is_some() {
            match js.delete_consumer_from_stream(&durable, &stream).await {
                Ok(_) => {}
                Err(error) if not_found(error.kind()) => {}
                Err(error) => return Err(unavailable(&durable, "delete", error)),
            }
        }
        let consumer = js
            .create_consumer_on_stream(config, stream.as_str())
            .await
            .map_err(|error| unavailable(&durable, "create", error))?;
        journal.event(
            event::READY,
            json!({
                "consumer": durable,
                "stream": stream,
                "settled_floor": settled_floor,
            }),
        );
        Ok(Self {
            consumer,
            journal,
            ledger: Ledger::default(),
            settled_floor,
            #[cfg(test)]
            stop_at: None,
        })
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// Records every record as it arrives, until a pull or a settle fails.
    pub async fn run(&mut self) -> Result<(), Down> {
        loop {
            self.next(PULL_EXPIRES).await?;
        }
    }

    /// Waits up to `expires` for one record and handles it. `Ok(false)` when none came.
    pub async fn next(&mut self, expires: Duration) -> Result<bool, Down> {
        let durable = self.consumer.cached_info().name.clone();
        let mut batch = self
            .consumer
            .fetch()
            .max_messages(1)
            .expires(expires)
            .messages()
            .await
            .map_err(|error| unavailable(&durable, "pull", error))?;
        let Some(message) = batch.next().await else {
            return Ok(false);
        };
        let message = message.map_err(|error| unavailable(&durable, "pull", error))?;
        let sequence = message
            .info()
            .map_err(|error| unavailable(&durable, "delivery metadata", error))?
            .stream_sequence;
        // The stop points apply to records this process writes a line for; a record an
        // earlier process settled only rebuilds the ledger.
        let fresh = sequence > self.settled_floor;
        if fresh {
            self.stop(Boundary::Read)?;
        }
        self.record(sequence, parse_record(message.headers.as_ref()));
        if fresh {
            self.stop(Boundary::Recorded)?;
        }
        // Settled only once the line is written: a record is never settled unrecorded.
        message
            .double_ack()
            .await
            .map_err(|error| unavailable(&durable, "ack", error))?;
        Ok(true)
    }

    fn record(&mut self, sequence: u64, record: Result<super::DeadRecord, &'static str>) {
        let fresh = sequence > self.settled_floor;
        match record {
            Ok(record) => {
                let classification = self.ledger.classify(&record.message_id, sequence);
                if !fresh {
                    return;
                }
                match classification {
                    Classification::First => self.journal.event(
                        event::RECORDED,
                        json!({
                            "message_id": record.message_id,
                            "stream_sequence": sequence,
                            "original_subject": record.original_subject,
                            "content_digest": record.content_digest,
                            "delivery_count": record.delivery_count,
                            "reason": record.reason,
                        }),
                    ),
                    Classification::Duplicate { first_sequence } => self.journal.event(
                        event::DUPLICATE,
                        json!({
                            "message_id": record.message_id,
                            "stream_sequence": sequence,
                            "first_sequence": first_sequence,
                        }),
                    ),
                }
            }
            Err(missing) if fresh => self.journal.event(
                event::MALFORMED,
                json!({ "stream_sequence": sequence, "missing_header": missing }),
            ),
            Err(_) => {}
        }
    }

    #[cfg(test)]
    fn stop(&self, boundary: Boundary) -> Result<(), Down> {
        if self.stop_at == Some(boundary) {
            return Err(Down::new("stopped", format!("stopped at {boundary:?}")));
        }
        Ok(())
    }

    #[cfg(not(test))]
    fn stop(&self, _boundary: Boundary) -> Result<(), Down> {
        Ok(())
    }
}

fn not_found(kind: ConsumerErrorKind) -> bool {
    matches!(kind, ConsumerErrorKind::JetStream(error) if error.error_code() == ErrorCode::CONSUMER_NOT_FOUND)
}

fn unavailable(durable: &str, act: &str, error: impl std::fmt::Display) -> Down {
    Down::new(
        cause::CONSUMER_UNAVAILABLE,
        format!("{durable} {act}: {error}"),
    )
}

/// Waits for bootstrap, then keeps `c_ckbus_dead` consumed for the life of the process.
/// A failure writes `ckbus.dead_letter.down` and the consumer is set up again (a fresh
/// replay) after one `period`.
async fn serve(mut ready: watch::Receiver<Option<Arc<Ready>>>, period: Duration) {
    let journal: Arc<dyn Journal> = Arc::new(StderrJournal);
    let ready = loop {
        if let Some(ready) = ready.borrow().clone() {
            break ready;
        }
        if ready.changed().await.is_err() {
            return;
        }
    };
    let down = |down: Down| {
        journal.event(
            event::DOWN,
            json!({
                "cause": down.cause,
                "message": down.message,
                "period_ms": period.as_millis() as u64,
            }),
        );
    };
    let Ok(names) = grants::derive_account(&ready.account.acct) else {
        down(Down::new(
            cause::NAMING_CONSTRUCTOR_ABSENT,
            format!("the account {} derives no names", ready.account.acct),
        ));
        return;
    };
    // The box-account connection bootstrap opened. The box plane hands it out as the
    // sentinel's link; the dead-letter consumer uses the same connection.
    let Some(link) = ready.box_plane.sentinel_link() else {
        down(Down::new(
            cause::LINK_ABSENT,
            "the box plane has no connection to consume on",
        ));
        return;
    };
    loop {
        let outcome = match DeadLetter::open(link.client.clone(), &names, journal.clone()).await {
            Ok(mut consumer) => consumer.run().await,
            Err(failure) => Err(failure),
        };
        if let Err(failure) = outcome {
            down(failure);
        }
        tokio::time::sleep(period).await;
    }
}

/// Starts the dead-letter consumer beside bootstrap.
pub fn wire(ready: watch::Receiver<Option<Arc<Ready>>>, period: Duration) {
    tokio::spawn(serve(ready, period));
}
