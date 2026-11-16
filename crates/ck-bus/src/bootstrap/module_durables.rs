//! The non-agent durables bootstrap creates: module readers named `m_{module_id}[_suffix]`
//! and dead-letter diagnostics named `c_ckbus_dead` by the naming crate.
//!
//! - `m_basal` on the event stream, filtered on every module event
//!   (`ck.{acct}.event.>`): the flow engine reads all of them through one cursor and
//!   drops the events no installed flow wants.
//! - The naming crate's closed event-consumer table, including prefrontal-core's
//!   broca reader and astrocyte's insula reader, on the event stream.
//! - `m_prefrontal-core` on the ROOM stream, filtered on the ROOM binding
//!   (`ck.{acct}.room.*.post`): prefrontal-core is the only consumer of room posts.
//!
//! All are ensured at every boot, before any module's first credential, so events and
//! posts published before the module first connects are kept for it. Modules hold pull, ack and info
//! on their own durable but no create, so ck-bus is the only party that can create them.
//!
//! Event-reader settings come from the naming crate's shared builder. The ROOM reader
//! uses the same pull settings. Reconciliation may update a durable, but never deletes
//! one: deleting it would discard the module's acknowledgement floor.
//! Reliable command readers on INBOX and CORE_WORK and diagnostics on both dead-letter
//! streams come from the naming crate's delivery-consumer table.

use cortexkit_bus_naming::{
    delivery_consumer_specs, event_consumer_specs, flow_engine_consumer_spec, shipped_streams,
    validate_consumer, AccountNames, AckPolicy, ConsumerSpec, DeliverPolicy,
};

use super::plane::{BoxPlane, DurableConsumer, PlaneError};
use crate::grants::{DELIVERY_AUTHORITY_MODULE, FLOW_ENGINE_MODULE};

/// The module durables, each filter checked against its stream's binding before the
/// server is asked. An error names the refused name or filter; it means the naming crate
/// and this plan disagree, which no retry clears.
pub fn planned(names: &AccountNames) -> Result<Vec<DurableConsumer>, String> {
    let flow =
        flow_engine_consumer_spec(names, FLOW_ENGINE_MODULE).map_err(|error| error.to_string())?;
    let mut room = flow.clone();
    room.consumer = ConsumerSpec {
        durable: AccountNames::module_consumer_name(DELIVERY_AUTHORITY_MODULE)
            .map_err(|error| error.to_string())?,
        stream: names.streams().room.clone(),
        filter_subjects: vec![names.room_binding()],
    };
    let table = event_consumer_specs(names).map_err(|error| error.to_string())?;
    let mut durables = std::iter::once(flow)
        .chain(table)
        .chain(std::iter::once(room))
        .map(|spec| {
            // These irrefutable patterns stop compiling if the naming crate adds an AckPolicy
            // or DeliverPolicy variant, so a new policy cannot silently be configured as the
            // explicit-ack, deliver-all pull consumer that ck-bus creates.
            let AckPolicy::Explicit = spec.ack_policy;
            let DeliverPolicy::All = spec.deliver_policy;
            if !spec.pull {
                return Err(format!("{} must be a pull consumer", spec.consumer.durable));
            }
            validate_consumer(&spec.consumer, &shipped_streams(names))
                .map_err(|error| error.to_string())?;
            Ok(DurableConsumer {
                stream: spec.consumer.stream,
                durable: spec.consumer.durable,
                filter_subjects: spec.consumer.filter_subjects,
                ack_wait: spec.ack_wait,
                // JetStream uses -1 for unlimited deliveries; the spec imposes no retry cap.
                max_deliver: -1,
                max_ack_pending: spec.max_ack_pending,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    for spec in delivery_consumer_specs(names).map_err(|error| error.to_string())? {
        let AckPolicy::Explicit = spec.ack_policy;
        let DeliverPolicy::All = spec.deliver_policy;
        if !spec.pull {
            return Err(format!("{} must be a pull consumer", spec.consumer.durable));
        }
        durables.push(DurableConsumer {
            stream: spec.consumer.stream,
            durable: spec.consumer.durable,
            filter_subjects: spec.consumer.filter_subjects,
            ack_wait: spec.ack_wait,
            max_deliver: spec.max_deliver,
            max_ack_pending: spec.max_ack_pending,
        });
    }
    Ok(durables)
}

/// Bootstrap's log rows and the number of retired cursors observed on the broker.
pub struct PlanOutput {
    pub rows: Vec<serde_json::Value>,
    pub orphaned_module_durables: usize,
}

/// Describes desired readers and retired module names without removing any cursor.
/// Only `m_` consumers on module streams belong to this reconciliation domain.
pub async fn plan_output(
    names: &AccountNames,
    plane: &dyn BoxPlane,
    desired: &[DurableConsumer],
) -> Result<PlanOutput, PlaneError> {
    let mut rows = desired
        .iter()
        .map(|row| {
            serde_json::json!({
                "stream": row.stream, "durable": row.durable,
                "filter_subjects": row.filter_subjects, "state": "desired",
            })
        })
        .collect::<Vec<_>>();
    let mut orphaned_module_durables = 0;
    for stream in [
        &names.streams().event,
        &names.streams().room,
        &names.streams().inbox,
        &names.streams().core_work,
    ] {
        for durable in plane.consumer_names(stream).await? {
            if durable.starts_with("m_")
                && !desired
                    .iter()
                    .any(|row| row.stream == *stream && row.durable == durable)
            {
                orphaned_module_durables += 1;
                rows.push(serde_json::json!({
                    "stream": stream, "durable": durable, "state": "orphaned",
                }));
            }
        }
    }
    Ok(PlanOutput {
        rows,
        orphaned_module_durables,
    })
}
