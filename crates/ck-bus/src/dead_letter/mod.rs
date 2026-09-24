//! The dead-letter consumer: ck-bus reads `CK_{ACCT}_EFFECT_DEAD` through its durable
//! `c_ckbus_dead` and records each dead-lettered message once per message id.
//!
//! Terminal disposition belongs to the claimant (foundation, "Terminal disposition has a
//! named executor"): on cap exhaustion it publishes a dead-letter record to
//! `ck.{acct}.effect.dead` and only then calls `term()` on the original item. A crash
//! between the two leaves the record stored and the item unterminated, and the next
//! claimant publishes the same record again. The stream drops a republish that carries
//! the same `Nats-Msg-Id` inside its duplicate window; a republish outside that window
//! is stored as a second message. ck-bus therefore deduplicates here, on the record's
//! own `message-id` field.
//!
//! What "recorded" means. The durable record is the stream message itself: the claimant
//! waited for the server to store it before terminating the item, so a terminated item
//! always has a stored record. ck-bus's record of it is one `ckbus.dead_letter.recorded`
//! line naming the message id and the stream sequence of the FIRST stored record for
//! that id. Every later stored record with the same id is a
//! `ckbus.dead_letter.duplicate` line naming both sequences. ck-bus settles (acks) a
//! record on `c_ckbus_dead` only after its line is written, so a record is never
//! settled unrecorded.
//!
//! Which record is first is a function of the stream alone, so the answer survives a
//! restart without a store file of its own (the durable store's shapes are closed). At
//! every start ck-bus reads how far the previous `c_ckbus_dead` had settled (its ack
//! floor), deletes it and creates it again from the first retained record. Replaying the
//! stream rebuilds the ledger of first sequences; records at or below the old ack floor
//! were recorded by an earlier process and are settled again without a line. A process
//! that dies after writing a line and before settling the record writes the same line
//! (same message id, same sequence) again after the restart: a repeat of one record,
//! never a second record for the id.
//!
//! The spec gives dead-letter state no place in the health answer, so none is added.

pub mod consumer;

use std::{
    collections::{hash_map::Entry, HashMap},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};

/// The dead-letter record's fields, as `cortexkit_bus_trait::DeadLetterRecord::headers`
/// writes them. A unit test pins the two together.
pub mod header {
    pub const ORIGINAL_SUBJECT: &str = "original-subject";
    pub const MESSAGE_ID: &str = "message-id";
    pub const CONTENT_DIGEST: &str = "content-digest";
    pub const DELIVERY_COUNT: &str = "delivery-count";
    pub const REASON: &str = "reason";
}

/// The id `AccountNames::consumer_name` turns into `c_ckbus_dead`.
pub const CONSUMER_ID: &str = "ckbus_dead";

/// `c_ckbus_dead`'s limits, each stated rather than left to a server default. The
/// foundation fixes pull and explicit ack for every durable and names no other value
/// for this one.
/// - Ack wait: the 30 s every durable in the plane uses.
/// - Max deliver: unlimited (-1). A record that could exhaust its deliveries would stop
///   being offered and never be recorded.
/// - Max ack pending: 1. Records are recorded one at a time in stream order, which is
///   what makes "first stored record for an id" well defined.
/// - Max waiting: 512 pull requests, the server's own default, stated. ck-bus keeps at
///   most one pull open.
pub const ACK_WAIT: Duration = Duration::from_secs(30);
pub const MAX_DELIVER: i64 = -1;
pub const MAX_ACK_PENDING: i64 = 1;
pub const MAX_WAITING: i64 = 512;
/// How long one pull waits for a record before it is renewed.
pub const PULL_EXPIRES: Duration = Duration::from_secs(5);

/// One stored dead-letter record, as read from its headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadRecord {
    pub message_id: String,
    pub original_subject: Option<String>,
    pub content_digest: Option<String>,
    pub delivery_count: Option<String>,
    pub reason: Option<String>,
}

/// Reads a record from its headers. A record without a `message-id` cannot be
/// deduplicated and is refused with the missing header named; the caller records it
/// as malformed rather than dropping it.
pub fn parse_record(headers: Option<&async_nats::HeaderMap>) -> Result<DeadRecord, &'static str> {
    let value = |name: &str| {
        headers
            .and_then(|headers| headers.get_last(name))
            .map(|value| value.as_str().to_string())
    };
    let message_id = value(header::MESSAGE_ID)
        .filter(|id| !id.is_empty())
        .ok_or(header::MESSAGE_ID)?;
    Ok(DeadRecord {
        message_id,
        original_subject: value(header::ORIGINAL_SUBJECT),
        content_digest: value(header::CONTENT_DIGEST),
        delivery_count: value(header::DELIVERY_COUNT),
        reason: value(header::REASON),
    })
}

/// Whether a stored record is the first for its message id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    First,
    Duplicate { first_sequence: u64 },
}

/// The lowest stream sequence seen for each message id.
#[derive(Debug, Default)]
pub struct Ledger {
    first: HashMap<String, u64>,
}

impl Ledger {
    /// Classifies the record stored at `sequence`. The same record offered twice (a
    /// redelivery) is still `First`.
    pub fn classify(&mut self, message_id: &str, sequence: u64) -> Classification {
        match self.first.entry(message_id.to_string()) {
            Entry::Vacant(entry) => {
                entry.insert(sequence);
                Classification::First
            }
            Entry::Occupied(mut entry) => {
                let first = *entry.get();
                if sequence < first {
                    // Records arrive in stream order, so this does not happen; if it
                    // did, the lower sequence is the first record.
                    entry.insert(sequence);
                    Classification::First
                } else if sequence == first {
                    Classification::First
                } else {
                    Classification::Duplicate {
                        first_sequence: first,
                    }
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        self.first.len()
    }

    pub fn is_empty(&self) -> bool {
        self.first.is_empty()
    }
}

/// Where the consumer writes its lines. The process writes them to stderr; a test can
/// collect them instead.
pub trait Journal: Send + Sync {
    fn event(&self, event: &str, fields: Value);
}

/// One structured stderr line, in the same shape as bootstrap's.
pub struct StderrJournal;

impl Journal for StderrJournal {
    fn event(&self, event: &str, fields: Value) {
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
}

/// The lines the consumer writes.
pub mod event {
    /// The consumer is in place and replaying from the first retained record.
    pub const READY: &str = "ckbus.dead_letter.ready";
    /// The first stored record for a message id.
    pub const RECORDED: &str = "ckbus.dead_letter.recorded";
    /// A later stored record for a message id already recorded.
    pub const DUPLICATE: &str = "ckbus.dead_letter.duplicate";
    /// A record without a message id: settled, never deduplicated.
    pub const MALFORMED: &str = "ckbus.dead_letter.malformed";
    /// The consumer stopped; it is set up again after one sentinel period.
    pub const DOWN: &str = "ckbus.dead_letter.down";
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_bus_trait::{
        ContentDigest, DeadLetterRecord, MaxDeliveriesExceeded, Message, WorkItem,
    };

    fn headers_of(record: &DeadLetterRecord) -> async_nats::HeaderMap {
        let mut headers = async_nats::HeaderMap::new();
        for (name, value) in record.headers() {
            headers.insert(name.as_str(), value.as_str());
        }
        headers
    }

    #[test]
    fn the_claimant_s_record_headers_parse() {
        let digest = ContentDigest::of_bytes(b"item");
        let record = DeadLetterRecord::from_exhaustion(&MaxDeliveriesExceeded {
            item: WorkItem {
                token: cortexkit_bus_trait::DeliveryToken(7),
                message: Message {
                    subject: "ck.box_a.effect.agent.s.intent".to_string(),
                    id: "effect-1".to_string(),
                    digest,
                    headers: Default::default(),
                },
                delivery_count: 5,
            },
            max_deliveries: 5,
        });
        let parsed = parse_record(Some(&headers_of(&record))).expect("a claimant record parses");
        assert_eq!(
            parsed,
            DeadRecord {
                message_id: "effect-1".to_string(),
                original_subject: Some("ck.box_a.effect.agent.s.intent".to_string()),
                content_digest: Some(digest.to_string()),
                delivery_count: Some("5".to_string()),
                reason: Some(record.reason.clone()),
            }
        );
    }

    #[test]
    fn a_record_without_a_message_id_is_refused_by_name() {
        assert_eq!(parse_record(None), Err(header::MESSAGE_ID));
        let mut headers = async_nats::HeaderMap::new();
        headers.insert(header::REASON, "max_deliveries_exceeded");
        assert_eq!(parse_record(Some(&headers)), Err(header::MESSAGE_ID));
    }

    #[test]
    fn the_first_sequence_for_an_id_is_the_record_and_later_ones_are_duplicates() {
        let mut ledger = Ledger::default();
        assert_eq!(ledger.classify("m1", 3), Classification::First);
        assert_eq!(ledger.classify("m2", 4), Classification::First);
        assert_eq!(
            ledger.classify("m1", 9),
            Classification::Duplicate { first_sequence: 3 }
        );
        // A redelivery of the first record is still the first record.
        assert_eq!(ledger.classify("m1", 3), Classification::First);
        assert_eq!(ledger.len(), 2);
    }
}
