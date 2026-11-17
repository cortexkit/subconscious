use super::*;
use runtime::SentinelHealth;

const OLD_DURABLE: &str = "m_prefrontal-core";
const NEW_DURABLE: &str = "m_prefrontal";

fn delivery_streams(plane: &UpgradePlane) -> [&str; 3] {
    let streams = plane.names.streams();
    [&streams.inbox, &streams.core_work, &streams.room]
}

fn old_bucket(plane: &UpgradePlane) -> String {
    plane
        .names
        .module_bucket_name("prefrontal-core", "session-liveness")
        .unwrap()
}

async fn seed_old_readers(plane: &UpgradePlane) {
    drop(plane.bootstrap().await);
    for spec in bootstrap::module_durables::planned(&plane.names)
        .unwrap()
        .into_iter()
        .filter(|spec| spec.durable == NEW_DURABLE)
    {
        plane
            .js
            .delete_consumer_from_stream(NEW_DURABLE, &spec.stream)
            .await
            .unwrap();
        plane
            .js
            .create_consumer_on_stream(
                pull::Config {
                    durable_name: Some(OLD_DURABLE.into()),
                    ack_policy: jetstream::consumer::AckPolicy::Explicit,
                    ack_wait: spec.ack_wait,
                    max_deliver: spec.max_deliver,
                    max_ack_pending: spec.max_ack_pending,
                    filter_subjects: spec
                        .filter_subjects
                        .iter()
                        .map(|filter| filter.replace(".v1.prefrontal", ".v1.prefrontal-core"))
                        .collect(),
                    ..Default::default()
                },
                &spec.stream,
            )
            .await
            .unwrap();
    }
    let bucket = old_bucket(plane);
    let current = cortexkit_bus_naming::MODULE_BUCKETS
        .iter()
        .find(|bucket| bucket.owner == "prefrontal" && bucket.name == "session-liveness")
        .unwrap();
    let mut config = bootstrap::plane::module_bucket_config(&plane.names, current).unwrap();
    config.name = format!("KV_{bucket}");
    config.subjects = vec![format!("$KV.{bucket}.>")];
    plane.js.create_stream(config).await.unwrap();
    plane
        .js
        .publish(format!("$KV.{bucket}.agent_keep"), "stale liveness".into())
        .await
        .unwrap()
        .await
        .unwrap();
    // An acknowledged EVENT cursor must survive even though delivery readers move.
    let sequence = plane.store_event("broca", "run").await;
    assert_eq!(plane.ack_one(OLD_DURABLE).await, sequence);
}

async fn old_reader(
    plane: &UpgradePlane,
    stream: &str,
) -> jetstream::consumer::Consumer<pull::Config> {
    plane
        .js
        .get_consumer_from_stream(OLD_DURABLE, stream)
        .await
        .unwrap()
}

async fn assert_old_bucket_retained(plane: &UpgradePlane) {
    let bucket = plane.js.get_key_value(old_bucket(plane)).await.unwrap();
    assert_eq!(
        bucket.get("agent_keep").await.unwrap().unwrap().as_ref(),
        b"stale liveness"
    );
}

#[tokio::test]
async fn empty_delivery_readers_move_and_stale_bucket_is_deleted_but_event_cursor_stays() {
    let _gate = harness::acceptance_gate().await;
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
    seed_old_readers(&plane).await;
    let event_before = plane.cursor(OLD_DURABLE).await;
    for stream in delivery_streams(&plane) {
        let info = old_reader(&plane, stream).await.get_info().await.unwrap();
        assert_eq!((info.num_pending, info.num_ack_pending), (0, 0));
    }
    let ready = plane.bootstrap().await;
    assert_eq!(ready.orphaned_module_durables, 1);
    for stream in delivery_streams(&plane) {
        assert!(ready
            .box_plane
            .consumer_state(stream, OLD_DURABLE)
            .await
            .unwrap()
            .is_none());
        let state = ready
            .box_plane
            .consumer_state(stream, NEW_DURABLE)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((state.num_pending, state.num_ack_pending), (0, 0));
    }
    assert!(plane
        .js
        .get_stream(format!("KV_{}", old_bucket(&plane)))
        .await
        .is_err());
    let current_bucket = plane
        .names
        .module_bucket_name("prefrontal", "session-liveness")
        .unwrap();
    assert!(plane.js.get_key_value(current_bucket).await.is_ok());
    assert_eq!(plane.cursor(OLD_DURABLE).await, event_before);
    plane.server.stop().await;
}

async fn refuses_busy_reader(ack_pending: bool) {
    let _gate = harness::acceptance_gate().await;
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
    seed_old_readers(&plane).await;
    let event_before = plane.cursor(OLD_DURABLE).await;
    for stream in delivery_streams(&plane) {
        let reader = old_reader(&plane, stream).await;
        let before = reader.get_info().await.unwrap();
        let filter = before.config.filter_subjects.first().unwrap();
        plane
            .js
            .publish(filter.replace('*', "rename"), "queued work".into())
            .await
            .unwrap()
            .await
            .unwrap();
        let message = if ack_pending {
            Some(
                reader
                    .fetch()
                    .max_messages(1)
                    .messages()
                    .await
                    .unwrap()
                    .next()
                    .await
                    .unwrap()
                    .unwrap(),
            )
        } else {
            None
        };
        let counts = if ack_pending { (0, 1) } else { (1, 0) };
        let info = reader.get_info().await.unwrap();
        assert_eq!((info.num_pending, info.num_ack_pending), counts);
        let failure = match boot(
            Some(&plane.machine_id),
            BrokerInputs {
                config: &plane.config,
                broker: &plane.broker,
            },
            &plane.deps,
        )
        .await
        {
            Err(failure) => failure,
            Ok(_) => panic!("busy old reader on {stream} must refuse the move"),
        };
        assert_eq!(
            failure.cause,
            bootstrap::cause::DELIVERY_DURABLE_MOVE_REFUSED
        );
        assert!(
            !failure.retry,
            "rollback requires an operator, not a background retry"
        );
        let health = bootstrap::BootstrapHealth::default();
        health.record_failure(failure);
        let report = health.report_health().await.unwrap();
        assert_eq!(report.status, subc_protocol::session::HealthStatus::Failing);
        for text in [
            stream.to_string(),
            format!("pending={}", counts.0),
            format!("ack_pending={}", counts.1),
        ] {
            assert!(
                report.detail.as_ref().unwrap().contains(&text),
                "{report:?}"
            );
            assert!(report.metrics.as_ref().unwrap()["message"]
                .as_str()
                .unwrap()
                .contains(&text));
        }
        let after = reader.get_info().await.unwrap();
        assert_eq!(after.created, before.created);
        assert_eq!((after.num_pending, after.num_ack_pending), counts);
        for untouched in delivery_streams(&plane) {
            assert!(plane
                .js
                .get_consumer_from_stream::<pull::Config, _, _>(NEW_DURABLE, untouched)
                .await
                .is_err());
            assert!(plane
                .js
                .get_consumer_from_stream::<pull::Config, _, _>(OLD_DURABLE, untouched)
                .await
                .is_ok());
        }
        assert_old_bucket_retained(&plane).await;
        assert_eq!(plane.cursor(OLD_DURABLE).await, event_before);
        // Drain only after checking refusal, so the next stream is tested independently.
        let message = match message {
            Some(message) => message,
            None => reader
                .fetch()
                .max_messages(1)
                .messages()
                .await
                .unwrap()
                .next()
                .await
                .unwrap()
                .unwrap(),
        };
        message.double_ack().await.unwrap();
    }
    plane.server.stop().await;
}

#[tokio::test]
async fn pending_delivery_refuses_move_reports_health_and_retains_old_bucket() {
    refuses_busy_reader(false).await;
}

#[tokio::test]
async fn ack_pending_delivery_refuses_move_reports_health_and_retains_old_bucket() {
    refuses_busy_reader(true).await;
}

#[tokio::test]
async fn second_start_keeps_new_delivery_cursors_and_bucket_while_old_bucket_stays_absent() {
    let _gate = harness::acceptance_gate().await;
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
    seed_old_readers(&plane).await;
    drop(plane.bootstrap().await);
    let mut before = Vec::new();
    for stream in delivery_streams(&plane) {
        let reader: jetstream::consumer::Consumer<pull::Config> = plane
            .js
            .get_consumer_from_stream(NEW_DURABLE, stream)
            .await
            .unwrap();
        let info = reader.get_info().await.unwrap();
        plane
            .js
            .publish(
                info.config.filter_subjects[0].replace('*', "rename"),
                "new work".into(),
            )
            .await
            .unwrap()
            .await
            .unwrap();
        let message = reader
            .fetch()
            .max_messages(1)
            .messages()
            .await
            .unwrap()
            .next()
            .await
            .unwrap()
            .unwrap();
        message.double_ack().await.unwrap();
        before.push((stream, reader.get_info().await.unwrap()));
    }
    let current_bucket = plane
        .names
        .module_bucket_name("prefrontal", "session-liveness")
        .unwrap();
    let bucket = plane.js.get_key_value(&current_bucket).await.unwrap();
    bucket
        .put("agent_new", "new liveness".into())
        .await
        .unwrap();
    let bucket_created = plane
        .js
        .get_stream(format!("KV_{current_bucket}"))
        .await
        .unwrap()
        .cached_info()
        .created;
    drop(plane.bootstrap().await);
    for (stream, before) in before {
        let reader: jetstream::consumer::Consumer<pull::Config> = plane
            .js
            .get_consumer_from_stream(NEW_DURABLE, stream)
            .await
            .unwrap();
        let after = reader.get_info().await.unwrap();
        assert_eq!(after.created, before.created);
        assert_eq!(after.ack_floor, before.ack_floor);
        assert_eq!(after.delivered, before.delivered);
        assert_eq!((after.num_pending, after.num_ack_pending), (0, 0));
        assert!(plane
            .js
            .get_consumer_from_stream::<pull::Config, _, _>(OLD_DURABLE, stream)
            .await
            .is_err());
    }
    assert_eq!(
        bucket.get("agent_new").await.unwrap().unwrap().as_ref(),
        b"new liveness"
    );
    assert_eq!(
        plane
            .js
            .get_stream(format!("KV_{current_bucket}"))
            .await
            .unwrap()
            .cached_info()
            .created,
        bucket_created
    );
    assert!(plane
        .js
        .get_stream(format!("KV_{}", old_bucket(&plane)))
        .await
        .is_err());
    plane.server.stop().await;
}
