// The legacy-route hot path: frames that cannot depend on the waiting queue
// are forwarded without the route's flow lock, so a client-to-module frame
// stays a header rewrite on routes without `busy_refusals` too.

fn legacy_frame(ty: FrameType, corr: u64) -> Frame {
    Frame::build(
        ty,
        Flags::new(false, Priority::Interactive, false),
        1,
        1,
        corr,
        Vec::new(),
    )
    .unwrap()
}

/// With the flow lock held elsewhere (the module reader releasing a credit,
/// say), a reverse reply and a CANCEL for a request that is not waiting are
/// still forwarded at once instead of waiting for the lock.
#[test]
fn reverse_replies_and_unqueued_frames_skip_the_flow_lock() {
    let flow = Arc::new(ChannelFlow::new(1));
    let held = flow.state.lock().unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let offering = Arc::clone(&flow);
    std::thread::spawn(move || {
        for (ty, corr) in [
            (FrameType::Response, 77),
            (FrameType::Error, 78),
            (FrameType::Cancel, 5),
            (FrameType::StreamData, 6),
        ] {
            let arrival = offering.offer_legacy(legacy_frame(ty, corr));
            let forwarded = matches!(
                arrival,
                LegacyArrival::Forward {
                    credited: false,
                    ..
                }
            );
            done_tx.send((ty, forwarded)).unwrap();
        }
    });
    for _ in 0..4 {
        let (ty, forwarded) = done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("a frame that cannot depend on the waiting queue took the flow lock");
        assert!(forwarded, "{ty:?} is forwarded at once");
    }
    drop(held);
}
