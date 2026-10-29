use super::*;

fn options(timeout_ms: u64) -> CallOptions {
    CallOptions {
        timeout: Duration::from_millis(timeout_ms),
        ..CallOptions::default()
    }
}

fn call(
    consumer: &SubcConsumer,
    timeout_ms: u64,
) -> impl Future<Output = Result<Vec<u8>, CallError>> + '_ {
    consumer.call(
        restart_target("plexus"),
        restart_identity(),
        b"work".to_vec(),
        options(timeout_ms),
    )
}

async fn accepted_request(
    shared: &Arc<Shared>,
    receiver: &mut mpsc::Receiver<WriteCommand>,
) -> Frame {
    let command = receiver.recv().await.unwrap();
    assert_eq!(command.frame.header.ty, FrameType::Request);
    assert_ne!(command.frame.header.channel, 0);
    assert!(shared.mark_pending_accepted(command.pending.unwrap()));
    command.frame
}

fn take_goodbyes(receiver: &mut mpsc::Receiver<WriteCommand>) -> Vec<Frame> {
    let mut goodbyes = Vec::new();
    while let Ok(command) = receiver.try_recv() {
        match command.frame.header.ty {
            FrameType::Goodbye => goodbyes.push(command.frame),
            FrameType::Ping => {}
            other => panic!("unexpected frame after deadline: {other:?}"),
        }
    }
    goodbyes
}

#[tokio::test(start_paused = true)]
async fn response_deadline_reopens_silent_route_without_resending() {
    let (shared, consumer, mut receiver) = route_open_stand_in(seeded_random(1));
    let (_, old) = open_answered(&shared, &consumer, &mut receiver, None, 41, "initial bind").await;
    let mut first = Box::pin(call(&consumer, 20));
    assert_pending(first.as_mut()).await;
    let request = accepted_request(&shared, &mut receiver).await;
    assert_eq!(request.header.channel, old.channel);
    tokio::time::advance(Duration::from_millis(20)).await;
    let err = first.await.unwrap_err();
    assert_eq!(err.outcome_cause(), Some(OutcomeUnknownCause::Deadline));
    let goodbyes = take_goodbyes(&mut receiver);

    let mut second = Box::pin(call(&consumer, 5_000));
    assert_pending(second.as_mut()).await;
    let (corr, _) = next_route_open(
        &mut receiver,
        "response deadline must retire the cached route",
    )
    .await;
    assert!(dispatch_frame(&shared, 1, route_open_answer(corr, 42, None)).await);
    assert_pending(second.as_mut()).await;
    let request = accepted_request(&shared, &mut receiver).await;
    assert_eq!(request.header.channel, 42);
    reply_to_drain_call(&shared, request).await;
    assert_eq!(second.await.unwrap(), b"work");
    assert_eq!(goodbyes.len(), 1);
    assert_eq!(
        (goodbyes[0].header.channel, goodbyes[0].header.epoch),
        (old.channel, old.epoch)
    );
    assert_eq!(shared.lock_inner().generation, old.connection_token());
}

#[tokio::test(start_paused = true)]
async fn response_deadline_preserves_sibling_and_goodbye_waits_for_last_pending() {
    let (shared, consumer, mut receiver) = route_open_stand_in(seeded_random(1));
    let (_, old) = open_answered(&shared, &consumer, &mut receiver, None, 41, "initial bind").await;
    let mut first = Box::pin(call(&consumer, 20));
    let mut second = Box::pin(call(&consumer, 200));
    assert_pending(first.as_mut()).await;
    assert_pending(second.as_mut()).await;
    let _first_request = accepted_request(&shared, &mut receiver).await;
    let second_request = accepted_request(&shared, &mut receiver).await;
    tokio::time::advance(Duration::from_millis(20)).await;
    assert_eq!(
        first.await.unwrap_err().outcome_cause(),
        Some(OutcomeUnknownCause::Deadline)
    );
    assert!(take_goodbyes(&mut receiver).is_empty());
    assert!(shared.validate_ingress_handle(1, old.channel, old.epoch));
    reply_to_drain_call(&shared, second_request).await;
    assert_eq!(second.await.unwrap(), b"work");
    let goodbyes = take_goodbyes(&mut receiver);
    assert_eq!(goodbyes.len(), 1);
    assert_eq!(goodbyes[0].header.channel, old.channel);
    assert!(!shared.validate_ingress_handle(1, old.channel, old.epoch));
    consumer.close().await;
    assert!(
        take_goodbyes(&mut receiver).is_empty(),
        "GOODBYE is sent once"
    );
}

#[tokio::test(start_paused = true)]
async fn response_deadline_retired_route_gets_goodbye_on_consumer_close() {
    let (shared, consumer, mut receiver) = route_open_stand_in(seeded_random(1));
    let (_, old) = open_answered(&shared, &consumer, &mut receiver, None, 41, "initial bind").await;
    let mut first = Box::pin(call(&consumer, 20));
    let mut second = Box::pin(call(&consumer, 200));
    assert_pending(first.as_mut()).await;
    assert_pending(second.as_mut()).await;
    accepted_request(&shared, &mut receiver).await;
    accepted_request(&shared, &mut receiver).await;
    tokio::time::advance(Duration::from_millis(20)).await;
    assert_eq!(
        first.await.unwrap_err().outcome_cause(),
        Some(OutcomeUnknownCause::Deadline)
    );
    assert!(take_goodbyes(&mut receiver).is_empty());
    consumer.close().await;
    second
        .await
        .expect_err("consumer close settles the pending sibling");
    let goodbyes = take_goodbyes(&mut receiver);
    assert_eq!(goodbyes.len(), 1);
    assert_eq!(goodbyes[0].header.channel, old.channel);
}

#[tokio::test(start_paused = true)]
async fn response_deadline_consumer_close_flushes_retired_goodbye_before_stopping_writer() {
    let (shared, consumer, mut receiver) = route_open_stand_in(seeded_random(1));
    let (_, old) = open_answered(&shared, &consumer, &mut receiver, None, 41, "initial bind").await;
    let mut first = Box::pin(call(&consumer, 20));
    let mut sibling = Box::pin(call(&consumer, 200));
    assert_pending(first.as_mut()).await;
    assert_pending(sibling.as_mut()).await;
    accepted_request(&shared, &mut receiver).await;
    accepted_request(&shared, &mut receiver).await;
    tokio::time::advance(Duration::from_millis(20)).await;
    assert_eq!(
        first.await.unwrap_err().outcome_cause(),
        Some(OutcomeUnknownCause::Deadline)
    );
    assert!(take_goodbyes(&mut receiver).is_empty());
    let writer = InstrumentedWriter::new(false);
    let observer = writer.clone();
    shared.lock_inner().writer_task = Some(tokio::spawn(writer_loop(
        Arc::clone(&shared),
        writer,
        receiver,
        1,
    )));
    consumer.close().await;
    sibling
        .await
        .expect_err("consumer close settles the pending sibling");
    tokio::task::yield_now().await;
    let bytes = observer.bytes();
    let frame = read_frame(&mut bytes.as_slice()).await.unwrap().unwrap();
    assert_eq!(frame.header.ty, FrameType::Goodbye);
    assert_eq!(
        (frame.header.channel, frame.header.epoch),
        (old.channel, old.epoch)
    );
    assert!(observer.flush_count() > 0);
}

#[tokio::test(start_paused = true)]
async fn response_deadline_after_caller_cancellation_does_not_retire_route() {
    let (shared, consumer, mut receiver) = route_open_stand_in(seeded_random(1));
    let (_, old) = open_answered(&shared, &consumer, &mut receiver, None, 41, "initial bind").await;
    let (cancel, reply) = consumer.call_cancellable(
        restart_target("plexus"),
        restart_identity(),
        b"work".to_vec(),
        options(20),
    );
    let mut reply = Box::pin(reply);
    assert_pending(reply.as_mut()).await;
    accepted_request(&shared, &mut receiver).await;
    assert_eq!(cancel.cancel().await, crate::CancelOutcome::Sent);
    assert_eq!(
        receiver.recv().await.unwrap().frame.header.ty,
        FrameType::Cancel
    );
    tokio::time::advance(Duration::from_millis(20)).await;
    assert_eq!(
        reply.await.unwrap_err().outcome_cause(),
        Some(OutcomeUnknownCause::Deadline)
    );
    assert!(take_goodbyes(&mut receiver).is_empty());
    assert_cached_call_succeeds(&shared, &consumer, &mut receiver, old).await;
}

async fn assert_cached_call_succeeds(
    shared: &Arc<Shared>,
    consumer: &SubcConsumer,
    receiver: &mut mpsc::Receiver<WriteCommand>,
    old: RouteHandle,
) {
    let mut next = Box::pin(call(consumer, 200));
    assert_pending(next.as_mut()).await;
    let request = accepted_request(shared, receiver).await;
    assert_eq!(
        (request.header.channel, request.header.epoch),
        (old.channel, old.epoch)
    );
    reply_to_drain_call(shared, request).await;
    assert_eq!(next.await.unwrap(), b"work");
}

#[tokio::test(start_paused = true)]
async fn response_deadline_flow_control_not_sent_does_not_retire_route() {
    let (shared, consumer, mut receiver) = route_open_stand_in(seeded_random(1));
    let (_, old) = open_answered(&shared, &consumer, &mut receiver, None, 41, "initial bind").await;
    let sem = shared.route_state(old).unwrap().sem;
    let permits = sem
        .clone()
        .acquire_many_owned(sem.available_permits() as u32)
        .await
        .unwrap();
    let mut blocked = Box::pin(call(&consumer, 20));
    assert_pending(blocked.as_mut()).await;
    tokio::time::advance(Duration::from_millis(20)).await;
    assert!(matches!(blocked.await.unwrap_err(), CallError::NotSent(_)));
    assert!(receiver.try_recv().is_err());
    drop(permits);
    assert_cached_call_succeeds(&shared, &consumer, &mut receiver, old).await;
}
