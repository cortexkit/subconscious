//! Agent durables (spec ruling R15): prefrontal owns where each agent resides, and it
//! creates, removes and reads each agent's durables through ops on ck-bus's management
//! surface. ck-bus accepts them only from `reserved:prefrontal-core`; any other caller,
//! a `Direct` one included, is refused `ckbus_caller_not_permitted`.
//!
//! An agent has one durable, `consumer_name(agent_id)` (`c_{agent_id}`), on each of the
//! three agent streams: WAKE (filter `wake_fire(agent)`), PEER (`peer_filter(agent)`) and
//! EFFECT (`effect_filter(agent)`). Every name and filter comes from the naming crate.
//!
//! - `ckbus.agent_durable_bind {agent_id}` creates the three durables with every limit
//!   stated. An existing durable with the same configuration is reported `created:
//!   false`; one with a different configuration is refused by name and left as it is.
//!   A refusal on one stream leaves the others as they are, and a retry converges.
//! - `ckbus.agent_durable_delete {agent_id}` deletes the three durables and purges the
//!   agent's subjects from their streams, so its undelivered messages are gone and a
//!   later bind does not deliver them again. An absent durable is not an error.
//! - `ckbus.agent_durables_list` reports one row per durable on an agent stream, with
//!   `pending` the server's count of messages the durable has not delivered yet.
//! - `ckbus.agent_effects_pending {agent_id}` reports the agent's EFFECT durable's
//!   intents not yet delivered as `pending`, and the delivered-and-unacked ones
//!   separately as `in_flight`. In-flight intents are not counted: the server keeps an
//!   intent that exhausted `max_deliver` (and went to the dead-letter subject) in flight
//!   until its ack wait passes, so counting them would let a poisoned intent hold a
//!   merge back. Prefrontal reads this before a merge.
//!
//! Merge is not a ck-bus op: copying messages is a workload publish, which ck-bus never
//! holds. Prefrontal performs it with the delivery-authority grant (R15).
//!
//! The pull grant on any agent durable is a whole-token `*` in the consumer position on
//! the three agent streams, so no consumer other than an agent's `c_{agent_id}` may ever
//! exist on them. `CKBUS_OWNED_DURABLES` lists the consumers ck-bus itself owns, and its
//! test holds them off the agent streams.

use std::{sync::Arc, time::Duration};

use cortexkit_bus_naming::{
    shipped_streams, validate_consumer, AccountNames, ConsumerSpec, NamingError, StreamKind,
};
use serde_json::{json, Value};
use subc_protocol::Principal;

use crate::{
    bootstrap::plane::{BoxPlane, ConsumerState, DurableConsumer},
    issuance::{code as issuance_code, handler::principal_label, log_event, PlaneSource, Refusal},
};

pub const BIND_OP: &str = "ckbus.agent_durable_bind";
pub const DELETE_OP: &str = "ckbus.agent_durable_delete";
pub const LIST_OP: &str = "ckbus.agent_durables_list";
pub const EFFECTS_PENDING_OP: &str = "ckbus.agent_effects_pending";

/// Every op this area serves, with whether it changes the plane.
pub const OPERATIONS: [(&str, bool); 4] = [
    (BIND_OP, true),
    (DELETE_OP, true),
    (LIST_OP, false),
    (EFFECTS_PENDING_OP, false),
];

/// The only module whose attested routes may call these ops.
pub const AUTHORIZED_MODULE: &str = "prefrontal-core";

pub mod code {
    pub const CALLER_NOT_PERMITTED: &str = "ckbus_caller_not_permitted";
    /// An existing durable's configuration differs from the one bind would create.
    pub const DURABLE_CONFLICT: &str = "ckbus_agent_durable_conflict";
    /// The broker refused or failed a durable, purge or listing request.
    pub const PLANE_FAILED: &str = "ckbus_agent_durable_failed";
}

/// The shipped durable workload consumer configuration (foundation, Constants): pull,
/// explicit ack, deliver all, ack wait 30 s, max-deliver 5 on the effect stream and
/// unlimited elsewhere.
pub const DURABLE_ACK_WAIT: Duration = Duration::from_secs(30);
pub const EFFECT_MAX_DELIVER: i64 = 5;
pub const UNLIMITED_MAX_DELIVER: i64 = -1;
/// The foundation names no ack-pending cap. It is set explicitly to the value
/// nats-server applies when none is given, so the limit is visible here rather than
/// inherited silently.
pub const DURABLE_MAX_ACK_PENDING: i64 = 1_000;
/// The server's names for the policies every agent durable is created with.
pub const ACK_POLICY: &str = "explicit";
pub const DELIVER_POLICY: &str = "all";

/// The three streams that hold agent durables.
pub const AGENT_STREAM_KINDS: [StreamKind; 3] =
    [StreamKind::Wake, StreamKind::Peer, StreamKind::Effect];

/// Consumers ck-bus creates for itself, by stream kind and name. None may sit on an agent
/// stream: the participant grant pulls any consumer there.
pub const CKBUS_OWNED_DURABLES: [(StreamKind, &str); 1] =
    [(StreamKind::EffectDead, "c_ckbus_dead")];

/// The literal names of the agent streams, as the naming crate lists them (the list its
/// whole-token `*` grants are written against).
pub fn agent_streams(names: &AccountNames) -> Vec<String> {
    names
        .streams()
        .agent_streams()
        .iter()
        .map(|stream| stream.to_string())
        .collect()
}

/// The literal name of the stream of `kind`.
pub fn stream_name(names: &AccountNames, kind: StreamKind) -> String {
    let streams = names.streams();
    match kind {
        StreamKind::Room => streams.room.clone(),
        StreamKind::Wake => streams.wake.clone(),
        StreamKind::Peer => streams.peer.clone(),
        StreamKind::Effect => streams.effect.clone(),
        StreamKind::EffectDead => streams.effect_dead.clone(),
    }
}

/// The three durables bind creates for `agent_id`, each filter checked against its
/// stream's binding.
pub fn agent_durables(names: &AccountNames, agent_id: &str) -> Result<Vec<DurableConsumer>, Refusal> {
    let naming = |error: NamingError| Refusal::new(issuance_code::NAME_REFUSED, error.to_string());
    let durable = AccountNames::consumer_name(agent_id).map_err(naming)?;
    let streams = names.streams();
    let planned = [
        (streams.wake.clone(), names.wake_fire(agent_id).map_err(naming)?, UNLIMITED_MAX_DELIVER),
        (streams.peer.clone(), names.peer_filter(agent_id).map_err(naming)?, UNLIMITED_MAX_DELIVER),
        (streams.effect.clone(), names.effect_filter(agent_id).map_err(naming)?, EFFECT_MAX_DELIVER),
    ];
    let shipped = shipped_streams(names);
    planned
        .into_iter()
        .map(|(stream, filter, max_deliver)| {
            let spec = ConsumerSpec {
                durable: durable.clone(),
                stream: stream.clone(),
                filter_subjects: vec![filter.clone()],
            };
            validate_consumer(&spec, &shipped)
                .map_err(|error| Refusal::new(issuance_code::NAME_REFUSED, error.to_string()))?;
            Ok(DurableConsumer {
                stream,
                durable: durable.clone(),
                filter_subjects: vec![filter],
                ack_wait: DURABLE_ACK_WAIT,
                max_deliver,
                max_ack_pending: DURABLE_MAX_ACK_PENDING,
            })
        })
        .collect()
}

/// How an existing consumer differs from the one bind would create, one entry per field,
/// each naming the field and both values. Empty when they match.
pub fn config_differences(existing: &ConsumerState, wanted: &DurableConsumer) -> Vec<String> {
    let mut differences = Vec::new();
    let found = &existing.config;
    let mut compare = |field: &str, found: String, wanted: String| {
        if found != wanted {
            differences.push(format!("{field} is {found}, bind creates {wanted}"));
        }
    };
    compare("filter_subjects", format!("{:?}", found.filter_subjects), format!("{:?}", wanted.filter_subjects));
    compare("ack_wait", format!("{:?}", found.ack_wait), format!("{:?}", wanted.ack_wait));
    compare("max_deliver", found.max_deliver.to_string(), wanted.max_deliver.to_string());
    compare("max_ack_pending", found.max_ack_pending.to_string(), wanted.max_ack_pending.to_string());
    compare("ack_policy", existing.ack_policy.clone(), ACK_POLICY.to_string());
    compare("deliver_policy", existing.deliver_policy.clone(), DELIVER_POLICY.to_string());
    differences
}

pub struct Membership {
    plane: Arc<dyn PlaneSource>,
}

impl Membership {
    pub fn new(plane: Arc<dyn PlaneSource>) -> Self {
        Self { plane }
    }

    /// Answers one request. `None` when `method` is not one of this area's ops.
    pub async fn answer(
        &self,
        principal: Option<&Principal>,
        method: &str,
        params: &Value,
    ) -> Option<Result<Value, Refusal>> {
        if !OPERATIONS.iter().any(|(name, _)| *name == method) {
            return None;
        }
        let outcome = match authorize(principal, method) {
            Err(refusal) => Err(refusal),
            Ok(()) => self.serve(method, params).await,
        };
        log_event(
            "ckbus.membership.answer",
            json!({
                "op": method,
                "principal": principal_label(principal),
                "agent_id": params.get("agent_id"),
                "outcome": match &outcome {
                    Ok(_) => "answered",
                    Err(refusal) => refusal.code,
                },
            }),
        );
        Some(outcome)
    }

    async fn serve(&self, method: &str, params: &Value) -> Result<Value, Refusal> {
        let plane = self.plane.current().ok_or_else(|| {
            Refusal::new(
                issuance_code::NOT_READY,
                "bootstrap has not finished; no box account connection yet",
            )
        })?;
        let box_plane = plane.box_plane.as_ref();
        match method {
            LIST_OP => list(&plane.names, box_plane).await,
            _ => {
                let agent_id = params.get("agent_id").and_then(Value::as_str).ok_or_else(|| {
                    Refusal::new(
                        issuance_code::BAD_REQUEST,
                        format!("{method} requires params.agent_id"),
                    )
                })?;
                match method {
                    BIND_OP => bind(&plane.names, box_plane, agent_id).await,
                    DELETE_OP => delete(&plane.names, box_plane, agent_id).await,
                    _ => effects_pending(&plane.names, box_plane, agent_id).await,
                }
            }
        }
    }
}

/// Only an attested `prefrontal-core` route passes. The principal is what the daemon
/// stamped on the route at bind time; nothing in the request body is consulted.
pub fn authorize(principal: Option<&Principal>, method: &str) -> Result<(), Refusal> {
    match principal {
        Some(Principal::Reserved { module_id }) if module_id == AUTHORIZED_MODULE => Ok(()),
        other => Err(Refusal::new(
            code::CALLER_NOT_PERMITTED,
            format!(
                "{method} answers only reserved:{AUTHORIZED_MODULE}; this route arrived as {}",
                principal_label(other)
            ),
        )),
    }
}

fn plane_failed(error: impl std::fmt::Display) -> Refusal {
    Refusal::new(code::PLANE_FAILED, error.to_string())
}

pub async fn bind(
    names: &AccountNames,
    plane: &dyn BoxPlane,
    agent_id: &str,
) -> Result<Value, Refusal> {
    let mut durables = Vec::new();
    for wanted in agent_durables(names, agent_id)? {
        let existing = plane
            .consumer_state(&wanted.stream, &wanted.durable)
            .await
            .map_err(plane_failed)?;
        let created = match existing {
            Some(existing) => {
                let differences = config_differences(&existing, &wanted);
                if !differences.is_empty() {
                    return Err(Refusal::new(
                        code::DURABLE_CONFLICT,
                        format!(
                            "durable {} on {} exists with a different configuration and is \
                             left as it is: {}",
                            wanted.durable,
                            wanted.stream,
                            differences.join("; ")
                        ),
                    ));
                }
                false
            }
            None => {
                plane.create_durable(&wanted).await.map_err(plane_failed)?;
                true
            }
        };
        durables.push(json!({
            "stream": wanted.stream,
            "durable": wanted.durable,
            "filter_subject": wanted.filter_subjects[0],
            "created": created,
        }));
    }
    Ok(json!({ "agent_id": agent_id, "durables": durables }))
}

/// Deletes each durable, then purges its filter from the stream. Deleting first means a
/// failure between the two leaves no durable to deliver the messages, and a retry finds
/// the durable absent and purges again.
pub async fn delete(
    names: &AccountNames,
    plane: &dyn BoxPlane,
    agent_id: &str,
) -> Result<Value, Refusal> {
    let mut deleted = Vec::new();
    let mut purged = serde_json::Map::new();
    for durable in agent_durables(names, agent_id)? {
        if plane
            .delete_durable(&durable.stream, &durable.durable)
            .await
            .map_err(plane_failed)?
        {
            deleted.push(durable.stream.clone());
        }
        let count = plane
            .purge_subject(&durable.stream, &durable.filter_subjects[0])
            .await
            .map_err(plane_failed)?;
        purged.insert(durable.stream.clone(), json!(count));
    }
    Ok(json!({ "agent_id": agent_id, "deleted": deleted, "purged": purged }))
}

/// One row per consumer on an agent stream. A consumer whose name is not an agent
/// durable (which the grant's invariant forbids) is still listed, with `agent_id` null,
/// so it is seen rather than hidden.
pub async fn list(names: &AccountNames, plane: &dyn BoxPlane) -> Result<Value, Refusal> {
    let mut rows = Vec::new();
    for stream in agent_streams(names) {
        let mut consumers = plane.consumer_names(&stream).await.map_err(plane_failed)?;
        consumers.sort();
        for durable in consumers {
            // Deleted between the listing and the read: not a durable any more.
            let Some(state) = plane
                .consumer_state(&stream, &durable)
                .await
                .map_err(plane_failed)?
            else {
                continue;
            };
            let agent_id = durable
                .strip_prefix("c_")
                .filter(|agent| AccountNames::consumer_name(agent).as_deref() == Ok(durable.as_str()));
            rows.push(json!({
                "agent_id": agent_id,
                "stream": stream,
                "durable": durable,
                "pending": state.num_pending,
                "filter_subject": state.config.filter_subjects.first(),
            }));
        }
    }
    Ok(Value::Array(rows))
}

pub async fn effects_pending(
    names: &AccountNames,
    plane: &dyn BoxPlane,
    agent_id: &str,
) -> Result<Value, Refusal> {
    let durable = agent_durables(names, agent_id)?
        .into_iter()
        .find(|durable| durable.stream == names.streams().effect)
        .expect("agent_durables plans a durable on the effect stream");
    let state = plane
        .consumer_state(&durable.stream, &durable.durable)
        .await
        .map_err(plane_failed)?;
    let (bound, undelivered, in_flight) = match &state {
        Some(state) => (true, state.num_pending, state.num_ack_pending),
        None => (false, 0, 0),
    };
    Ok(json!({
        "agent_id": agent_id,
        "stream": durable.stream,
        "durable": durable.durable,
        "bound": bound,
        "undelivered": undelivered,
        "in_flight": in_flight,
        "pending": undelivered,
    }))
}
