//! Retire the old delivery readers only after all three are empty. EVENT keeps its
//! old cursor: it is a separate reader, not a delivery queue to transfer.

use cortexkit_bus_naming::{AccountNames, MODULE_BUCKETS};
use serde_json::json;

use super::{cause, log_event, plane::BoxPlane, Failure};

const OLD_MODULE: &str = "prefrontal-core";
const OLD_DURABLE: &str = "m_prefrontal-core";
const LIVENESS_BUCKET: &str = "session-liveness";

pub(super) async fn remove_empty_delivery_readers(
    names: &AccountNames,
    plane: &dyn BoxPlane,
) -> Result<(), Failure> {
    let streams = names.streams();
    let mut empty = Vec::new();
    // Inspect every old reader before deleting any: a refusal must retain the
    // liveness bucket and the queued work for a rollback to the old authority.
    for stream in [&streams.inbox, &streams.core_work, &streams.room] {
        let state = plane
            .consumer_state(stream, OLD_DURABLE)
            .await
            .map_err(|error| Failure::retry(cause::MODULE_DURABLES_UNAVAILABLE, error.message))?;
        if let Some(state) = state {
            if state.num_pending != 0 || state.num_ack_pending != 0 {
                return Err(Failure::stop(
                    cause::DELIVERY_DURABLE_MOVE_REFUSED,
                    format!(
                        "delivery durable move refused on {stream}: {OLD_DURABLE} has pending={} ack_pending={}; old reader and liveness bucket retained",
                        state.num_pending, state.num_ack_pending,
                    ),
                ));
            }
            empty.push(stream);
        }
    }
    for stream in empty {
        // Delivery streams allow only one reader per subject, so the old durable
        // must be deleted before bootstrap can create its replacement.
        plane
            .delete_durable(stream, OLD_DURABLE)
            .await
            .map_err(|error| Failure::retry(cause::MODULE_DURABLES_UNAVAILABLE, error.message))?;
        log_event(
            "ckbus.bootstrap.delivery_durable_retired",
            json!({ "stream": stream, "durable": OLD_DURABLE }),
        );
    }
    Ok(())
}

pub(super) async fn remove_stale_liveness(
    names: &AccountNames,
    plane: &dyn BoxPlane,
) -> Result<(), Failure> {
    // This bucket is stale once the stopped authority's delivery readers have
    // moved. Never turn this one-time retirement into deletion of a current row.
    if MODULE_BUCKETS
        .iter()
        .any(|bucket| bucket.owner == OLD_MODULE && bucket.name == LIVENESS_BUCKET)
    {
        return Err(Failure::stop(
            cause::STREAMS_UNAVAILABLE,
            "refusing to retire prefrontal-core.session-liveness: still in the naming table",
        ));
    }
    if plane
        .delete_module_bucket(names, OLD_MODULE, LIVENESS_BUCKET)
        .await
        .map_err(|error| Failure::retry(cause::STREAMS_UNAVAILABLE, error.message))?
    {
        log_event(
            "ckbus.bootstrap.module_bucket_retired",
            json!({ "owner": OLD_MODULE, "bucket": LIVENESS_BUCKET }),
        );
    }
    Ok(())
}
