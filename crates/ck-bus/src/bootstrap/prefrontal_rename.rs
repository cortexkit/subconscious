//! Reconcile legacy delivery resources only when exactly one authority is registered.
//! EVENT and the federation-only PEER and WAKE readers have independent cursors.

use std::{path::Path, time::Duration};

use cortexkit_bus_naming::AccountNames;
use serde_json::json;
use subc_client_rs::{ConsumerOptions, SubcConsumer};
use tokio::sync::oneshot;

use crate::grants::{
    liveness_bucket, DELIVERY_AUTHORITY_MODULE, LEGACY_DELIVERY_AUTHORITY_MODULE, LIVENESS_BUCKET,
};

use super::{log_event, module_durables, plane::PlaneError, Ready};

pub(crate) fn delivery_stream(names: &AccountNames, stream: &str) -> bool {
    let streams = names.streams();
    [&streams.inbox, &streams.core_work, &streams.room]
        .iter()
        .any(|name| name.as_str() == stream)
}

fn owner<'a>(registered: impl IntoIterator<Item = &'a str>) -> Option<&'static str> {
    let (mut old, mut new) = (false, false);
    for module in registered {
        old |= module == LEGACY_DELIVERY_AUTHORITY_MODULE;
        new |= module == DELIVERY_AUTHORITY_MODULE;
    }
    match (old, new) {
        (true, false) => Some(LEGACY_DELIVERY_AUTHORITY_MODULE),
        (false, true) => Some(DELIVERY_AUTHORITY_MODULE),
        _ => None,
    }
}

pub(crate) async fn reconcile<'a>(
    ready: &Ready,
    registered: impl IntoIterator<Item = &'a str>,
) -> Result<(), PlaneError> {
    let Some(owner) = owner(registered) else {
        return Ok(());
    };
    let other = if owner == DELIVERY_AUTHORITY_MODULE {
        LEGACY_DELIVERY_AUTHORITY_MODULE
    } else {
        DELIVERY_AUTHORITY_MODULE
    };
    let names = &AccountNames::derive(&ready.account.acct)
        .map_err(|error| PlaneError::new(error.to_string()))?;
    let plane = ready.box_plane.as_ref();
    let other_durable =
        AccountNames::module_consumer_name(other).map_err(|e| PlaneError::new(e.to_string()))?;
    let target =
        AccountNames::module_consumer_name(owner).map_err(|e| PlaneError::new(e.to_string()))?;
    let bucket =
        liveness_bucket(owner).ok_or_else(|| PlaneError::new("missing session-liveness spec"))?;
    plane.ensure_module_bucket(names, &bucket).await?;
    for mut spec in module_durables::planned(names).map_err(PlaneError::new)? {
        if !delivery_stream(names, &spec.stream) {
            continue;
        }
        spec.durable.clone_from(&target);
        let previous = plane.consumer_state(&spec.stream, &other_durable).await?;
        let existing = plane.consumer_state(&spec.stream, &target).await?;
        let room = spec.stream == names.streams().room;
        if !room && previous.is_some() {
            for state in previous.iter().chain(existing.iter()) {
                let filters = &state.config.filter_subjects;
                if filters.len() != spec.filter_subjects.len()
                    || !spec
                        .filter_subjects
                        .iter()
                        .all(|filter| filters.contains(filter))
                {
                    return Err(PlaneError::new(format!(
                        "{} on {} has unexpected workqueue filters",
                        state.config.durable, spec.stream
                    )));
                }
            }
        }
        match (&previous, existing) {
            (Some(previous), None) if room => {
                // Limits retention keeps acknowledged posts. Start after the old ack
                // floor, creating first so a failed create can't discard the cursor.
                let next = previous
                    .ack_floor_stream_sequence
                    .and_then(|seq| seq.checked_add(1))
                    .ok_or_else(|| PlaneError::new("missing ROOM consumer ack floor"))?;
                plane.create_durable_from_sequence(&spec, next).await?;
            }
            (Some(previous), None) => {
                // The workqueue owns the messages, not the consumer. NATS won't
                // admit two consumers with these filters, so delete first.
                let deleted = plane.delete_durable(&spec.stream, &other_durable).await?;
                let mut replacement = plane.ensure_module_durable(&spec).await;
                for attempt in 1..=3 {
                    let Err(error) = &replacement else { break };
                    log_event(
                        "ckbus.bootstrap.delivery_durable_create_retry",
                        json!({"stream": spec.stream, "durable": target, "retry": attempt, "error": error.message}),
                    );
                    replacement = plane.ensure_module_durable(&spec).await;
                }
                if deleted {
                    log_move(
                        &spec.stream,
                        &other_durable,
                        &target,
                        previous,
                        room,
                        replacement.is_ok(),
                    );
                }
                replacement?;
                continue;
            }
            (None, None) => plane.ensure_module_durable(&spec).await?,
            (_, Some(_)) => {} // Keep the selected reader's existing cursor and start policy.
        }
        if let Some(previous) = previous {
            if plane.delete_durable(&spec.stream, &other_durable).await? {
                log_move(&spec.stream, &other_durable, &target, &previous, room, true);
            }
        }
    }
    if plane
        .delete_module_bucket(names, other, LIVENESS_BUCKET)
        .await?
    {
        log_event(
            "ckbus.bootstrap.module_bucket_retired",
            json!({"owner": other, "bucket": LIVENESS_BUCKET}),
        );
    }
    Ok(())
}

fn log_move(
    stream: &str,
    from: &str,
    to: &str,
    state: &super::plane::ConsumerState,
    room: bool,
    replacement_created: bool,
) {
    log_event(
        "ckbus.bootstrap.delivery_durable_moved",
        json!({
            "stream": stream, "from": from, "to": to,
            "pending": state.num_pending, "ack_pending": state.num_ack_pending,
            "room_ack_floor": state.ack_floor_stream_sequence.filter(|_| room),
            "room_redelivery_possible": room && state.num_ack_pending > 0,
            "replacement_available": replacement_created,
        }),
    );
}

pub(super) async fn follow(
    connection: &Path,
    ready: &Ready,
    initial: oneshot::Sender<()>,
    retry: Duration,
) {
    let mut initial = Some(initial);
    loop {
        let result = follow_connection(connection, ready, &mut initial, retry).await;
        log_event(
            "ckbus.bootstrap.delivery_catalog_unavailable",
            json!({"error": result}),
        );
        tokio::time::sleep(retry).await;
    }
}

async fn follow_connection(
    connection: &Path,
    ready: &Ready,
    initial: &mut Option<oneshot::Sender<()>>,
    retry: Duration,
) -> String {
    let consumer = match SubcConsumer::connect(connection, ConsumerOptions::default()).await {
        Ok(consumer) => consumer,
        Err(error) => return error.to_string(),
    };
    let result = async {
        let mut watch = consumer
            .catalog_watch()
            .await
            .map_err(|error| error.to_string())?;
        let mut observed = None;
        let mut pending = false;
        loop {
            let retrying = tokio::select! {
                event = watch.next() => {
                    if event.map_err(|error| error.to_string())?.is_none() {
                        break;
                    }
                    pending
                }
                _ = tokio::time::sleep(retry), if pending => true,
            };
            let catalog = consumer
                .catalog_list()
                .await
                .map_err(|error| error.to_string())?;
            let registered = || {
                catalog
                    .modules
                    .iter()
                    .map(|module| module.module_id.as_str())
            };
            let current = owner(registered());
            if observed != Some(current) {
                if retrying {
                    log_event(
                        "ckbus.bootstrap.delivery_reconcile_retry",
                        json!({"owner": current}),
                    );
                }
                match reconcile(ready, registered()).await {
                    Ok(()) => observed = Some(current),
                    Err(error) => {
                        // The move may already have deleted a reader. Recheck even if
                        // the catalog returns to the previously reconciled owner.
                        observed = None;
                        log_event(
                            "ckbus.bootstrap.delivery_reconcile_failed",
                            json!({"owner": current, "error": error.message}),
                        );
                    }
                }
            }
            pending = observed != Some(current);
            if let Some(done) = initial.take() {
                let _ = done.send(());
            }
        }
        Err::<(), _>("catalog.watch ended".to_owned())
    }
    .await;
    consumer.close().await;
    result.unwrap_err()
}
