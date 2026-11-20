// Timing measurements for two cost findings of a read-only router audit:
// connection teardown and drain polling scan every route in the table, and a
// busy refusal scans every held credit. They print figures and assert nothing
// about speed, so they are ignored and run on demand:
// `cargo test --release -p subc-daemon --lib --features test-support audit_measurements -- --ignored --nocapture --test-threads=1`

/// A forwarding table holding `routes` committed client routes to one module,
/// spread over client connections of 100 routes each, plus a second module
/// endpoint with a single route of its own. Returns the table, the second
/// endpoint, and the receivers that keep the client sinks open.
fn audit_route_table(
    routes: usize,
) -> (
    ForwardingTable,
    ModuleEndpointId,
    Vec<mpsc::Receiver<crate::router::OutboundFrame>>,
) {
    const ROUTES_PER_CLIENT: usize = 100;
    let forwarding = ForwardingTable::default();
    let busy_module = ConnectionId::new(1);
    let (busy_tx, busy_rx) = mpsc::channel(8);
    forwarding
        .register_module_connection(
            busy_module,
            "busy-fleet".to_string(),
            2,
            Concurrency::ModuleManaged,
            FrameSink::new(busy_tx),
        )
        .unwrap();
    let draining_module = ConnectionId::new(2);
    let (draining_tx, draining_rx) = mpsc::channel(8);
    let draining = forwarding
        .register_module_connection(
            draining_module,
            "draining".to_string(),
            2,
            Concurrency::ModuleManaged,
            FrameSink::new(draining_tx),
        )
        .unwrap();
    let mut receivers = Vec::new();
    let bind = |client: ConnectionId,
                sink: &FrameSink,
                corr: u64,
                module_id: &str,
                module_connection: ConnectionId| {
        let pending = forwarding
            .begin_route_bind_relay_for_test(client, sink.clone(), corr, module_id)
            .unwrap();
        forwarding
            .complete_pending_relay(
                module_connection,
                pending.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .unwrap();
    };
    let mut client_id = 1_000u64;
    let mut made = 0usize;
    while made < routes {
        client_id += 1;
        let (tx, rx) = mpsc::channel(ROUTES_PER_CLIENT + 1);
        let sink = FrameSink::new(tx);
        for corr in 0..ROUTES_PER_CLIENT.min(routes - made) {
            bind(
                ConnectionId::new(client_id),
                &sink,
                corr as u64,
                "busy-fleet",
                busy_module,
            );
            made += 1;
        }
        receivers.push(rx);
    }
    let (tx, rx) = mpsc::channel(2);
    bind(
        ConnectionId::new(client_id + 1),
        &FrameSink::new(tx),
        0,
        "draining",
        draining_module,
    );
    receivers.push(rx);
    receivers.push(busy_rx);
    receivers.push(draining_rx);
    assert_eq!(forwarding.active_binding_count().unwrap(), routes + 1);
    (forwarding, draining, receivers)
}

/// Teardown of a connection that never held a route, and one drain poll of an
/// endpoint that holds one route, at two sizes of unrelated route table.
#[test]
#[ignore = "timing measurement for audit finding 8, run on demand"]
fn audit_measurements_teardown_and_drain_poll_against_unrelated_routes() {
    const CALLS: u64 = 2_000;
    for routes in [1_000usize, 20_000] {
        let (forwarding, draining, _receivers) = audit_route_table(routes);
        let started = std::time::Instant::now();
        for connection in 0..CALLS {
            let released = forwarding
                .cleanup_connection(ConnectionId::new(1_000_000 + connection))
                .unwrap();
            assert!(released.is_empty());
        }
        let teardown = started.elapsed();
        let started = std::time::Instant::now();
        for _ in 0..CALLS {
            assert_eq!(forwarding.endpoint_in_flight_count(draining).unwrap(), 0);
        }
        let poll = started.elapsed();
        println!(
            "finding 8: {routes} unrelated routes: routeless connection teardown {:.1} us/call, \
             one-route endpoint drain poll {:.1} us/call ({CALLS} calls each)",
            teardown.as_nanos() as f64 / CALLS as f64 / 1_000.0,
            poll.as_nanos() as f64 / CALLS as f64 / 1_000.0,
        );
    }
}

/// One busy refusal on a full busy-refusal route, at the module-managed and
/// stateless-parallel window sizes (32 and 1,024 held credits).
#[test]
#[ignore = "timing measurement for audit finding 9, run on demand"]
fn audit_measurements_busy_refusal_against_held_credits() {
    const REFUSALS: u64 = 100_000;
    for window in [DEFAULT_MODULE_MANAGED_WINDOW, STATELESS_PARALLEL_WINDOW] {
        let flow = ChannelFlow::with_admission(window, true, None);
        for corr in 0..window as u64 {
            assert!(matches!(
                flow.try_acquire_tagged(corr, false),
                RequestAdmission::Admitted
            ));
        }
        let started = std::time::Instant::now();
        for corr in 0..REFUSALS {
            let admission = flow.try_acquire_tagged(window as u64 + corr, false);
            assert!(matches!(admission, RequestAdmission::Busy(_)));
            std::hint::black_box(admission);
        }
        let elapsed = started.elapsed();
        println!(
            "finding 9: {window} held credits: {:.0} ns per busy refusal ({REFUSALS} refusals)",
            elapsed.as_nanos() as f64 / REFUSALS as f64,
        );
    }
}
