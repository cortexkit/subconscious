use super::*;

const OLD_DURABLE: &str = "m_prefrontal-core";
const NEW_DURABLE: &str = "m_prefrontal";

#[tokio::test]
async fn fresh_boot_without_a_registered_owner_leaves_delivery_resources_absent() {
    let _gate = harness::acceptance_gate().await;
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
    let ready = plane.bootstrap().await;
    for stream in delivery_streams(&plane) {
        for durable in [OLD_DURABLE, NEW_DURABLE] {
            assert!(
                ready
                    .box_plane
                    .consumer_state(stream, durable)
                    .await
                    .unwrap()
                    .is_none(),
                "boot created {durable} on {stream} without a registered owner"
            );
        }
    }
    for owner in ["prefrontal-core", "prefrontal"] {
        let bucket = plane
            .names
            .module_bucket_name(owner, "session-liveness")
            .unwrap();
        assert!(plane.js.get_key_value(bucket).await.is_err());
    }
    plane.server.stop().await;
}

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
    drop(plane.bootstrap_with_owner("prefrontal-core").await);
    let store = plane.js.get_key_value(old_bucket(plane)).await.unwrap();
    store
        .put("agent_keep", "stale liveness".into())
        .await
        .unwrap();
    // EVENT is not one of the transferred delivery readers.
    plane
        .js
        .create_consumer_on_stream(
            pull::Config {
                durable_name: Some(OLD_DURABLE.into()),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                filter_subject: format!("ck.{}.event.broca.>", plane.names.account()),
                ..Default::default()
            },
            &plane.names.streams().event,
        )
        .await
        .unwrap();
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
    let ready = plane.bootstrap_with_owner("prefrontal").await;
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

#[tokio::test]
async fn neither_or_both_registered_preserve_the_existing_delivery_resources() {
    let _gate = harness::acceptance_gate().await;
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
    seed_old_readers(&plane).await;
    let ready = plane.bootstrap().await;
    let before = old_reader(&plane, &plane.names.streams().inbox)
        .await
        .get_info()
        .await
        .unwrap();
    for modules in [vec![], vec!["prefrontal-core", "prefrontal"]] {
        bootstrap::prefrontal_rename::reconcile(&ready, modules)
            .await
            .unwrap();
        for stream in delivery_streams(&plane) {
            assert!(ready
                .box_plane
                .consumer_state(stream, NEW_DURABLE)
                .await
                .unwrap()
                .is_none());
            assert!(ready
                .box_plane
                .consumer_state(stream, OLD_DURABLE)
                .await
                .unwrap()
                .is_some());
        }
        let after = old_reader(&plane, &plane.names.streams().inbox)
            .await
            .get_info()
            .await
            .unwrap();
        assert_eq!(after.created, before.created);
        assert_old_bucket_retained(&plane).await;
    }
    plane.server.stop().await;
}

#[tokio::test]
async fn rollback_moves_busy_workqueues_without_losing_pending_or_unacked_messages() {
    let _gate = harness::acceptance_gate().await;
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
    let before = plane.bootstrap_with_owner("prefrontal").await;
    let streams = plane.names.streams();
    for stream in [&streams.inbox, &streams.core_work] {
        let reader: jetstream::consumer::Consumer<pull::Config> = plane
            .js
            .get_consumer_from_stream(NEW_DURABLE, stream)
            .await
            .unwrap();
        let state = before
            .box_plane
            .consumer_state(stream, NEW_DURABLE)
            .await
            .unwrap()
            .unwrap();
        let subject = state.config.filter_subjects[0].replace('*', "move");
        for body in ["unacked", "pending"] {
            plane
                .js
                .publish(subject.clone(), body.into())
                .await
                .unwrap()
                .await
                .unwrap();
        }
        let first = pull_one(&reader).await;
        assert_eq!(first.payload.as_ref(), b"unacked");
        let info = reader.get_info().await.unwrap();
        assert_eq!((info.num_pending, info.num_ack_pending), (1, 1));
    }
    let ready = plane.bootstrap_with_owner("prefrontal-core").await;
    for stream in delivery_streams(&plane) {
        assert!(ready
            .box_plane
            .consumer_state(stream, NEW_DURABLE)
            .await
            .unwrap()
            .is_none());
        assert!(ready
            .box_plane
            .consumer_state(stream, OLD_DURABLE)
            .await
            .unwrap()
            .is_some());
    }
    for stream in [&streams.inbox, &streams.core_work] {
        let reader = old_reader(&plane, stream).await;
        for expected in [b"unacked".as_slice(), b"pending".as_slice()] {
            let message = pull_one(&reader).await;
            assert_eq!(message.payload.as_ref(), expected);
            message.double_ack().await.unwrap();
        }
    }
    let owner = plane.module_client("prefrontal-core").await;
    let own_store = jetstream::new(owner.client.clone())
        .get_key_value(old_bucket(&plane))
        .await
        .unwrap();
    assert!(own_store.get("agent_new").await.unwrap().is_none());
    own_store.put("agent_old", "liveness".into()).await.unwrap();
    let current_bucket = plane
        .names
        .module_bucket_name("prefrontal", "session-liveness")
        .unwrap();
    assert!(plane.js.get_key_value(current_bucket).await.is_err());
    plane.server.stop().await;
}

async fn pull_one(reader: &jetstream::consumer::Consumer<pull::Config>) -> jetstream::Message {
    reader
        .fetch()
        .max_messages(1)
        .expires(Duration::from_secs(2))
        .messages()
        .await
        .unwrap()
        .next()
        .await
        .expect("one delivery")
        .unwrap()
}

#[tokio::test]
async fn room_replacement_starts_after_the_ack_floor_not_at_the_first_retained_post() {
    let _gate = harness::acceptance_gate().await;
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
    let ready = plane.bootstrap_with_owner("prefrontal-core").await;
    let room = &plane.names.streams().room;
    for body in ["old", "next"] {
        plane
            .js
            .publish(plane.names.room_post("room_move").unwrap(), body.into())
            .await
            .unwrap()
            .await
            .unwrap();
    }
    let previous = old_reader(&plane, room).await;
    let old = pull_one(&previous).await;
    old.double_ack().await.unwrap();
    let floor = previous.get_info().await.unwrap().ack_floor.stream_sequence;
    assert!(floor > 0);
    assert_eq!(
        plane
            .js
            .get_stream(room)
            .await
            .unwrap()
            .cached_info()
            .state
            .messages,
        2
    );
    bootstrap::prefrontal_rename::reconcile(&ready, ["prefrontal"])
        .await
        .unwrap();
    let replacement: jetstream::consumer::Consumer<pull::Config> = plane
        .js
        .get_consumer_from_stream(NEW_DURABLE, room)
        .await
        .unwrap();
    assert_eq!(
        replacement.get_info().await.unwrap().config.deliver_policy,
        jetstream::consumer::DeliverPolicy::ByStartSequence {
            start_sequence: floor + 1
        }
    );
    let next = pull_one(&replacement).await;
    assert_eq!(next.info().unwrap().stream_sequence, floor + 1);
    assert_eq!(next.payload.as_ref(), b"next");
    assert!(ready
        .box_plane
        .consumer_state(room, OLD_DURABLE)
        .await
        .unwrap()
        .is_none());
    plane.server.stop().await;
}

#[tokio::test]
async fn second_start_keeps_new_delivery_cursors_and_bucket_while_old_bucket_stays_absent() {
    let _gate = harness::acceptance_gate().await;
    let Some(plane) = UpgradePlane::start().await else {
        return;
    };
    seed_old_readers(&plane).await;
    drop(plane.bootstrap_with_owner("prefrontal").await);
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
    drop(plane.bootstrap_with_owner("prefrontal").await);
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
