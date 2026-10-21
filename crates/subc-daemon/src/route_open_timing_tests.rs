use std::future::Future;

use crate::router::test_log::{captured_logs, log_capture};

async fn open_with_ack_delay(delay: Duration, scope_ref: Option<&str>) -> String {
    let mut rig = rig().await;
    let scope = scope_ref.map(|scope_ref| rig.selector(scope_ref, Some(1)));
    if let Some(scope_ref) = scope_ref {
        let mut record = head(scope_ref, 1);
        record.carriers.push(ScopeCarrier::new(Principal::Direct));
        rig.sync(vec![record]).await;
    }
    let (output, subscriber) = log_capture(tracing::Level::INFO);
    let (client, mut client_rx, frame) = rig.open_frame(None, PLEXUS, scope);
    let corr = frame.header.corr;
    let started_at = Instant::now().into_std();
    let mut open = Box::pin(rig.handler.handle_control_frame_timed(
        &client,
        frame,
        Some(started_at),
    ));

    // Poll through admission, filesystem resolution and send before advancing
    // the paused clock. Only the target's missing ACK can consume the delay.
    std::future::poll_fn(|cx| {
        assert!(open.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    let (module, module_rx) = rig.modules.get_mut(PLEXUS).unwrap();
    let bind = module_rx.try_recv().expect("bind sent to target").frame;
    tokio::time::advance(delay).await;
    rig.handler
        .handle_control_frame(module, route_bind_ack(bind.header.corr))
        .await
        .unwrap();
    assert!(open.await.unwrap().is_empty());
    let response = client_rx.try_recv().unwrap().frame;
    assert_eq!(response.header.corr, corr);
    assert_eq!(response.header.ty, FrameType::Response);
    drop(subscriber);
    captured_logs(&output)
}

fn logged_millis(line: &str, field: &str) -> u64 {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(field))
        .unwrap_or_else(|| panic!("missing numeric {field} in {line}"))
        .parse()
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn slow_route_open_warning_names_target_principal_and_dominant_ack_phase() {
    let logs = open_with_ack_delay(Duration::from_secs(2), Some("s")).await;
    let warnings = logs
        .lines()
        .filter(|line| line.contains("slow control dispatch"))
        .collect::<Vec<_>>();
    assert_eq!(
        warnings.len(),
        1,
        "one existing warning, not a second event"
    );
    let warning = warnings[0];
    assert!(warning.contains("WARN"), "{warning}");
    for field in [
        "op=route.open",
        "module_id=\"plexus\"",
        "principal=direct",
        "connection_id=101",
        "corr=101",
    ] {
        assert!(warning.contains(field), "missing {field} in {warning}");
    }
    assert_eq!(logged_millis(warning, "elapsed_ms="), 2000);
    let ack = logged_millis(warning, "bind_ack_wait_ms=");
    assert_eq!(ack, 2000);
    for phase in [
        "admission_ms",
        "project_root_ms",
        "egress_reservation_ms",
        "bind_send_ms",
    ] {
        let elapsed = logged_millis(warning, &format!("{phase}="));
        assert_eq!(elapsed, 0, "no clock advance during {phase}");
        assert!(ack > elapsed, "ACK wait must dominate {phase}");
    }
}

#[tokio::test(start_paused = true)]
async fn fast_route_open_emits_no_slow_dispatch_warning() {
    let logs = open_with_ack_delay(Duration::ZERO, Some("s")).await;
    assert!(!logs.contains("slow control dispatch"), "{logs}");
    assert!(!logs.contains("bind_ack_wait_ms="), "{logs}");
    assert!(logs.contains("route.open accepted"), "{logs}");
}

#[tokio::test(start_paused = true)]
async fn route_open_accept_log_records_scoped_and_unscoped_opens() {
    let unscoped_logs = open_with_ack_delay(Duration::ZERO, None).await;
    let unscoped = unscoped_logs
        .lines()
        .find(|line| line.contains("route.open accepted"))
        .expect("accepted route.open log");
    assert!(unscoped.contains("scope=none"), "{unscoped}");

    let scope_ref = "scope-reference-123456789";
    let scoped_logs = open_with_ack_delay(Duration::ZERO, Some(scope_ref)).await;
    let scoped = scoped_logs
        .lines()
        .find(|line| line.contains("route.open accepted"))
        .expect("accepted route.open log");
    assert!(
        scoped.contains(r#"scope=head/reserved:prefrontal-core/"scope-reference-123456789"@1"#),
        "{scoped}"
    );
    assert!(!scoped.contains("bind_proof"), "{scoped}");
    assert!(!scoped.contains("attributes"), "{scoped}");

    // A long ref is cut to 64 characters so one open cannot flood the line.
    let long_ref = "r".repeat(100);
    let long_logs = open_with_ack_delay(Duration::ZERO, Some(&long_ref)).await;
    let long = long_logs
        .lines()
        .find(|line| line.contains("route.open accepted"))
        .expect("accepted route.open log");
    assert!(
        long.contains(&format!("/\"{}…\"@1", "r".repeat(64))),
        "{long}"
    );
    assert!(!long.contains(&"r".repeat(65)), "{long}");
}
