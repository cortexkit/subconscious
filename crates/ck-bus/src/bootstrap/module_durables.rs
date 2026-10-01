//! The module durables bootstrap creates: one durable per module that reads a
//! non-agent stream, named `m_{module_id}` by the naming crate.
//!
//! - `m_basal` on the event stream, filtered on every module event
//!   (`ck.{acct}.event.>`): the flow engine reads all of them through one cursor and
//!   drops the events no installed flow wants.
//! - `m_prefrontal-core` on the ROOM stream, filtered on the ROOM binding
//!   (`ck.{acct}.room.*.post`): prefrontal-core is the only consumer of room posts.
//!
//! Both are ensured at every boot (created when absent, an identical one left as it is),
//! before either module's first credential, so events and posts published before the
//! module first connects are kept for it. The modules themselves hold pull, ack and info
//! on their own durable but no create, so ck-bus is the only party that can create them.
//!
//! The settings follow the foundation's workload-consumer configuration: pull, explicit
//! ack, deliver all, a 30-second ack wait, unlimited deliveries (only the effect stream
//! caps them, at 5) and at most 1000 unacknowledged deliveries. Agent durables use the
//! same settings (`membership::DURABLE_ACK_WAIT` and its neighbours).

use std::time::Duration;

use cortexkit_bus_naming::{shipped_streams, validate_consumer, AccountNames, ConsumerSpec};

use super::plane::DurableConsumer;
use crate::grants::{DELIVERY_AUTHORITY_MODULE, FLOW_ENGINE_MODULE};

pub const ACK_WAIT: Duration = Duration::from_secs(30);
pub const MAX_DELIVER: i64 = -1;
pub const MAX_ACK_PENDING: i64 = 1_000;

/// The two module durables, each filter checked against its stream's binding before the
/// server is asked. An error names the refused name or filter; it means the naming crate
/// and this plan disagree, which no retry clears.
pub fn planned(names: &AccountNames) -> Result<Vec<DurableConsumer>, String> {
    let streams = names.streams();
    let shipped = shipped_streams(names);
    [
        (
            FLOW_ENGINE_MODULE,
            streams.event.clone(),
            names.event_binding(),
        ),
        (
            DELIVERY_AUTHORITY_MODULE,
            streams.room.clone(),
            names.room_binding(),
        ),
    ]
    .into_iter()
    .map(|(module_id, stream, filter)| {
        let durable =
            AccountNames::module_consumer_name(module_id).map_err(|error| error.to_string())?;
        let spec = ConsumerSpec {
            durable: durable.clone(),
            stream: stream.clone(),
            filter_subjects: vec![filter],
        };
        validate_consumer(&spec, &shipped).map_err(|error| error.to_string())?;
        Ok(DurableConsumer {
            stream,
            durable,
            filter_subjects: spec.filter_subjects,
            ack_wait: ACK_WAIT,
            max_deliver: MAX_DELIVER,
            max_ack_pending: MAX_ACK_PENDING,
        })
    })
    .collect()
}
