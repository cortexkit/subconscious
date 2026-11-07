use super::*;

#[test]
fn route_busy_watch_limit_rolls_back_total_credit_and_never_exceeds_window() {
    let flow = ChannelFlow::with_admission(32, true, Some(DEFAULT_MODULE_MANAGED_WATCH_WINDOW));
    for corr in 1..=24 {
        assert_eq!(
            flow.try_acquire_tagged(corr, true),
            RequestAdmission::Admitted
        );
    }
    let RequestAdmission::Busy(detail) = flow.try_acquire_tagged(25, true) else {
        panic!("a 25th watch must be refused");
    };
    assert_eq!((detail.request_in_flight, detail.watch_in_flight), (24, 24));
    assert_eq!((detail.request_window, detail.watch_window), (32, Some(24)));
    assert_eq!(
        flow.available_permits(),
        8,
        "failed watch returned its total credit"
    );
    for corr in 25..=32 {
        assert_eq!(
            flow.try_acquire_tagged(corr, false),
            RequestAdmission::Admitted
        );
    }
    assert!(matches!(
        flow.try_acquire_tagged(33, false),
        RequestAdmission::Busy(_)
    ));
    assert_eq!(flow.in_flight(), 32);
    flow.release_corr(1);
    assert_eq!(
        flow.try_acquire_tagged(34, true),
        RequestAdmission::Admitted
    );
    assert_eq!(flow.in_flight(), 32);
    for corr in 2..=34 {
        flow.release_corr(corr);
    }
    assert_eq!(flow.available_permits(), 32);
    assert_eq!(flow.watch_sem.as_ref().unwrap().available_permits(), 24);
}

#[test]
fn route_busy_fast_and_repeated_terminals_conserve_both_lanes() {
    let flow = ChannelFlow::with_admission(32, true, Some(DEFAULT_MODULE_MANAGED_WATCH_WINDOW));
    for corr in 1..=128 {
        assert_eq!(
            flow.try_acquire_tagged(corr, true),
            RequestAdmission::Admitted
        );
        flow.release_corr(corr);
        flow.release_corr(corr);
        assert_eq!(flow.available_permits(), 32);
        assert_eq!(flow.watch_sem.as_ref().unwrap().available_permits(), 24);
    }
    assert_eq!(flow.in_flight(), 0);
}

#[test]
fn route_busy_serial_and_parallel_have_only_a_total_lane() {
    for concurrency in [Concurrency::Serial, Concurrency::StatelessParallel] {
        let window = window_for(&concurrency);
        let flow = ChannelFlow::with_admission(window, true, None);
        for corr in 0..window as u64 {
            assert_eq!(
                flow.try_acquire_tagged(corr, true),
                RequestAdmission::Admitted
            );
        }
        let RequestAdmission::Busy(detail) = flow.try_acquire_tagged(u64::MAX, true) else {
            panic!("full route must be refused");
        };
        assert_eq!(detail.watch_window, None);
        assert_eq!(detail.watch_in_flight, window);
        assert_eq!(detail.request_in_flight, window);
    }
}

#[test]
fn route_busy_close_and_drain_close_both_semaphores() {
    for drain in [false, true] {
        let flow = ChannelFlow::with_admission(32, true, Some(DEFAULT_MODULE_MANAGED_WATCH_WINDOW));
        assert_eq!(flow.try_acquire_tagged(1, true), RequestAdmission::Admitted);
        if drain {
            assert_eq!(flow.begin_drain(), 1);
        } else {
            flow.close();
        }
        assert!(flow.sem.is_closed());
        assert!(flow.watch_sem.as_ref().unwrap().is_closed());
        assert_eq!(flow.try_acquire_tagged(2, false), RequestAdmission::Stale);
        flow.release_corr(1);
        flow.release_corr(1);
        assert_eq!(flow.in_flight(), 0);
    }
}

struct AdmissionRaceFixture {
    forwarding: Arc<ForwardingTable>,
    route: Arc<RouteBinding>,
    _client_rx: tokio::sync::mpsc::Receiver<crate::router::OutboundFrame>,
    _module_rx: tokio::sync::mpsc::Receiver<crate::router::OutboundFrame>,
}

fn admission_race_fixture() -> AdmissionRaceFixture {
    let forwarding = Arc::new(ForwardingTable::default());
    let (module_tx, module_rx) = tokio::sync::mpsc::channel(64);
    let module_connection = ConnectionId::new(500);
    forwarding
        .register_module_connection(
            module_connection,
            "busy-race".into(),
            2,
            Concurrency::ModuleManaged,
            FrameSink::new(module_tx),
        )
        .unwrap();
    let (client_tx, client_rx) = tokio::sync::mpsc::channel(64);
    let pending = forwarding
        .begin_route_bind_relay_with_busy_for_test(
            ConnectionId::new(501),
            FrameSink::new(client_tx),
            1,
            "busy-race",
            true,
        )
        .unwrap();
    forwarding
        .complete_pending_relay(
            module_connection,
            pending.corr,
            RouteBindRelayOutcome::Accepted,
        )
        .unwrap();
    let DataRoute::Client(DataRouteState::Bound(route)) = forwarding
        .lookup_data_route(
            ConnectionId::new(501),
            pending.client_channel,
            pending.client_epoch,
        )
        .unwrap()
    else {
        panic!("accepted route must be published");
    };
    AdmissionRaceFixture {
        forwarding,
        route,
        _client_rx: client_rx,
        _module_rx: module_rx,
    }
}

#[tokio::test]
async fn route_busy_close_winning_admission_race_is_reloading_or_stale_not_busy() {
    for drain in [false, true] {
        let fixture = admission_race_fixture();
        for corr in 0..32 {
            assert_eq!(
                fixture
                    .forwarding
                    .try_admit_request(&fixture.route, fixture.route.client_epoch, corr, false)
                    .unwrap(),
                RequestAdmission::Admitted
            );
        }
        let mut writer = fixture.forwarding.write_inner().unwrap();
        std::thread::scope(|scope| {
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let reader_barrier = Arc::clone(&barrier);
            let forwarding = Arc::clone(&fixture.forwarding);
            let route = Arc::clone(&fixture.route);
            let admission = scope.spawn(move || {
                reader_barrier.wait();
                forwarding
                    .try_admit_request(&route, route.client_epoch, 33, false)
                    .unwrap()
            });
            barrier.wait();
            // The writer already owns the forwarding lock. The reader cannot
            // inspect the full window until close has made the binding unusable.
            if drain {
                let module = writer.modules_by_id["busy-race"].clone();
                begin_drain_locked(&mut writer, "busy-race", module, RouteCloseReason::Reload);
            } else {
                release_client_route_locked(
                    &mut writer,
                    ClientRouteKey {
                        connection_id: fixture.route.client_connection_id,
                        channel: fixture.route.client_channel,
                    },
                    fixture.route.client_epoch,
                );
            }
            drop(writer);
            assert_eq!(
                admission.join().unwrap(),
                if drain {
                    RequestAdmission::ModuleReloading
                } else {
                    RequestAdmission::Stale
                }
            );
        });
    }
}
