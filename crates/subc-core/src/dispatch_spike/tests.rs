use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Barrier,
    },
    time::Duration,
};

use subc_protocol::{Flags, FrameType, Priority};
use tokio::{sync::mpsc, time::timeout};

use super::*;

fn request(corr: u64, body: &[u8]) -> Frame {
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        7,
        11,
        corr,
        body.to_vec(),
    )
    .expect("test frame must be valid")
}

fn cancel(corr: u64) -> Frame {
    Frame::build(
        FrameType::Cancel,
        Flags::new(false, Priority::Interactive, false),
        7,
        11,
        corr,
        Vec::new(),
    )
    .expect("test cancel frame must be valid")
}

#[derive(Debug)]
struct CountingFlow {
    cap: usize,
    acquires: AtomicUsize,
    releases: AtomicUsize,
    in_flight: AtomicUsize,
}

impl CountingFlow {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            acquires: AtomicUsize::new(0),
            releases: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
        }
    }

    fn acquire(&self) {
        let old = self.in_flight.fetch_add(1, Ordering::AcqRel);
        assert!(old < self.cap, "acquire exceeded the configured window");
        self.acquires.fetch_add(1, Ordering::AcqRel);
    }

    fn assert_safe(&self) {
        let acquires = self.acquires.load(Ordering::Acquire);
        let releases = self.releases.load(Ordering::Acquire);
        let in_flight = self.in_flight.load(Ordering::Acquire);
        assert!(releases <= acquires, "release without an acquire");
        assert!(in_flight <= self.cap, "in-flight exceeded the window");
        assert_eq!(in_flight, acquires - releases);
    }

    fn assert_balanced(&self) {
        self.assert_safe();
        assert_eq!(self.in_flight.load(Ordering::Acquire), 0);
        assert_eq!(
            self.acquires.load(Ordering::Acquire),
            self.releases.load(Ordering::Acquire)
        );
    }
}

impl CreditRelease for Arc<CountingFlow> {
    fn release_credit(&self) {
        let old = self.in_flight.fetch_sub(1, Ordering::AcqRel);
        assert!(old > 0, "credit was released more than once");
        self.releases.fetch_add(1, Ordering::AcqRel);
    }
}

#[derive(Debug, Clone, Copy)]
enum Event {
    Push,
    Pop,
    Cancel,
    Commit,
    Terminal,
    Teardown,
}

struct InterleavingHarness {
    inbox: RouteInbox,
    flow: Arc<CountingFlow>,
    token: Option<CreditToken<Arc<CountingFlow>>>,
    entered: bool,
    synthetic_terminals: usize,
    module_terminals: usize,
}

impl InterleavingHarness {
    fn new() -> Self {
        Self {
            inbox: RouteInbox::new(2),
            flow: Arc::new(CountingFlow::new(1)),
            token: None,
            entered: false,
            synthetic_terminals: 0,
            module_terminals: 0,
        }
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Push => {
                let outcome = self.inbox.push_request(41, request(41, b"original"));
                if outcome == PushOutcome::Admitted {
                    self.entered = true;
                }
            }
            Event::Pop => {
                if self.inbox.pop_for_dispatch() == Some(41) {
                    self.flow.acquire();
                    self.token = Some(CreditToken::new(Arc::clone(&self.flow)));
                }
            }
            Event::Cancel => {
                if self.inbox.on_cancel(41) == CancelDecision::SynthesizeCancelled {
                    self.synthetic_terminals += 1;
                }
            }
            Event::Commit => self.commit(),
            Event::Terminal => self.terminal(),
            Event::Teardown => {
                self.inbox.begin_closing();
                self.synthetic_terminals += self.inbox.finish_closed().len();
            }
        }
        self.assert_safety();
    }

    fn commit(&mut self) {
        let Some(token) = self.token.take() else {
            return;
        };
        match self.inbox.commit_delivered(41) {
            CommitDecision::Proceed(_) => token.commit(),
            CommitDecision::RollbackCancelled => {
                drop(token);
                self.synthetic_terminals += 1;
            }
            CommitDecision::Missing => {
                drop(token);
                self.synthetic_terminals += 1;
            }
        }
    }

    fn terminal(&mut self) {
        if self.inbox.on_terminal(41) {
            self.flow.release_credit();
            self.module_terminals += 1;
        }
    }

    fn assert_safety(&self) {
        assert!(
            self.synthetic_terminals + self.module_terminals <= 1,
            "a correlation produced more than one terminal"
        );
        self.flow.assert_safe();
    }

    fn settle(mut self) {
        if !self.entered {
            assert_eq!(self.synthetic_terminals + self.module_terminals, 0);
            self.flow.assert_balanced();
            return;
        }

        for _ in 0..4 {
            match self.inbox.state(41) {
                Some(SlotState::Queued) => {
                    if self.inbox.pop_for_dispatch() == Some(41) {
                        self.flow.acquire();
                        self.token = Some(CreditToken::new(Arc::clone(&self.flow)));
                    }
                }
                Some(SlotState::Sending { .. }) => self.commit(),
                Some(SlotState::Delivered) => self.terminal(),
                None => break,
            }
            self.assert_safety();
        }

        assert!(
            (self.synthetic_terminals == 1) ^ (self.module_terminals == 1),
            "an admitted correlation must have exactly one synthetic xor module terminal"
        );
        assert_eq!(self.inbox.slot_count(), 0);
        self.flow.assert_balanced();
    }
}

fn visit_permutations(events: &mut [Event], start: usize, count: &mut usize) {
    if start == events.len() {
        let mut harness = InterleavingHarness::new();
        for event in events.iter().copied() {
            harness.apply(event);
        }
        harness.settle();
        *count += 1;
        return;
    }

    for index in start..events.len() {
        events.swap(start, index);
        visit_permutations(events, start + 1, count);
        events.swap(start, index);
    }
}

#[test]
fn exhaustive_sync_decision_interleavings_conserve_terminal_and_credit() {
    let mut events = [
        Event::Push,
        Event::Pop,
        Event::Cancel,
        Event::Commit,
        Event::Terminal,
        Event::Teardown,
    ];
    let mut count = 0;
    visit_permutations(&mut events, 0, &mut count);
    assert_eq!(count, 720, "all 6! event orderings must be explored");
}

#[test]
fn atomic_pop_claim_defers_sending_cancel_without_a_missing_slot() {
    let mut inbox = RouteInbox::new(1);
    assert_eq!(
        inbox.push_request(1, request(1, b"one")),
        PushOutcome::Admitted
    );
    assert_eq!(inbox.pop_for_dispatch(), Some(1));
    assert_eq!(
        inbox.state(1),
        Some(SlotState::Sending { cancelled: false })
    );
    assert_eq!(inbox.on_cancel(1), CancelDecision::DeferredToDrain);
    assert_eq!(inbox.commit_delivered(1), CommitDecision::RollbackCancelled);
    assert_eq!(inbox.slot_count(), 0);
}

#[test]
fn sending_commit_boundary_has_one_owner_in_both_orders() {
    let flow = Arc::new(CountingFlow::new(1));

    let mut cancel_first = RouteInbox::new(1);
    assert_eq!(
        cancel_first.push_request(1, request(1, b"cancel-first")),
        PushOutcome::Admitted
    );
    assert_eq!(cancel_first.pop_for_dispatch(), Some(1));
    flow.acquire();
    let token = CreditToken::new(Arc::clone(&flow));
    assert_eq!(cancel_first.on_cancel(1), CancelDecision::DeferredToDrain);
    assert_eq!(
        cancel_first.commit_delivered(1),
        CommitDecision::RollbackCancelled
    );
    drop(token);
    flow.assert_balanced();

    let mut commit_first = RouteInbox::new(1);
    assert_eq!(
        commit_first.push_request(2, request(2, b"commit-first")),
        PushOutcome::Admitted
    );
    assert_eq!(commit_first.pop_for_dispatch(), Some(2));
    flow.acquire();
    let token = CreditToken::new(Arc::clone(&flow));
    assert!(matches!(
        commit_first.commit_delivered(2),
        CommitDecision::Proceed(_)
    ));
    token.commit();
    assert_eq!(commit_first.on_cancel(2), CancelDecision::ForwardToModule);
    assert!(commit_first.on_terminal(2));
    flow.release_credit();
    flow.assert_balanced();
}

#[test]
fn duplicate_corr_is_rejected_without_overwriting_the_original_slot() {
    let mut inbox = RouteInbox::new(2);
    assert_eq!(
        inbox.push_request(7, request(7, b"original")),
        PushOutcome::Admitted
    );
    assert_eq!(
        inbox.push_request(7, request(7, b"replacement")),
        PushOutcome::DuplicateCorr
    );
    assert_eq!(inbox.slot_count(), 1);
    assert_eq!(inbox.pop_for_dispatch(), Some(7));
    let CommitDecision::Proceed(frame) = inbox.commit_delivered(7) else {
        panic!("original request should remain dispatchable");
    };
    assert_eq!(frame.body, b"original");
}

#[test]
fn consuming_credit_token_selects_exactly_one_release_owner() {
    let flow = Arc::new(CountingFlow::new(1));

    flow.acquire();
    drop(CreditToken::new(Arc::clone(&flow)));
    flow.assert_balanced();

    flow.acquire();
    CreditToken::new(Arc::clone(&flow)).commit();
    assert_eq!(flow.releases.load(Ordering::Acquire), 1);
    flow.release_credit();
    flow.assert_balanced();
}

#[tokio::test]
async fn real_channel_flow_token_rolls_back_commit_and_panic_exactly_once() {
    let flow = Arc::new(ChannelFlow::new(1));

    flow.acquire().await.expect("window is open");
    drop(CreditToken::new(Arc::clone(&flow)));
    assert_eq!(flow.in_flight(), 0);
    assert_eq!(flow.available_permits(), 1);

    flow.acquire().await.expect("rollback returned the permit");
    CreditToken::new(Arc::clone(&flow)).commit();
    assert_eq!(flow.in_flight(), 1);
    flow.release();
    assert_eq!(flow.in_flight(), 0);

    flow.acquire().await.expect("terminal returned the permit");
    let panic = catch_unwind(AssertUnwindSafe({
        let flow = Arc::clone(&flow);
        move || {
            let _token = CreditToken::new(flow);
            panic!("injected panic between acquire and commit");
        }
    }));
    assert!(panic.is_err());
    assert_eq!(flow.in_flight(), 0);
    assert_eq!(flow.available_permits(), 1);
}

fn dispatcher_fixture(
    window: usize,
    depth: usize,
) -> (
    Arc<RouteDispatcher>,
    RouteDrain,
    Arc<ChannelFlow>,
    mpsc::Receiver<crate::router::OutboundFrame>,
    mpsc::UnboundedReceiver<SyntheticTerminal>,
) {
    let flow = Arc::new(ChannelFlow::new(window));
    let (module_tx, module_rx) = mpsc::channel(depth.max(1));
    let (synthetic_tx, synthetic_rx) = mpsc::unbounded_channel();
    let (dispatcher, drain) = RouteDispatcher::new(
        depth,
        Arc::clone(&flow),
        FrameSink::new(module_tx),
        synthetic_tx,
    );
    (dispatcher, drain, flow, module_rx, synthetic_rx)
}

async fn prepare_delivered(dispatcher: &RouteDispatcher, flow: &Arc<ChannelFlow>, corr: u64) {
    assert_eq!(
        dispatcher.push_request(corr, request(corr, &[corr as u8])),
        PushOutcome::Admitted
    );
    assert_eq!(
        dispatcher
            .inbox
            .lock()
            .expect("route inbox mutex poisoned")
            .pop_for_dispatch(),
        Some(corr)
    );
    flow.acquire().await.expect("window is open");
    let token = CreditToken::new(Arc::clone(flow));
    assert!(matches!(
        dispatcher
            .inbox
            .lock()
            .expect("route inbox mutex poisoned")
            .commit_delivered(corr),
        CommitDecision::Proceed(_)
    ));
    token.commit();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_terminal_with_second_in_flight_cannot_steal_its_credit() {
    let (dispatcher, _drain, flow, _module_rx, _synthetic_rx) = dispatcher_fixture(2, 2);
    prepare_delivered(&dispatcher, &flow, 1).await;
    prepare_delivered(&dispatcher, &flow, 2).await;
    assert_eq!(flow.in_flight(), 2);
    assert_eq!(flow.available_permits(), 0);

    let barrier = Arc::new(Barrier::new(3));
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let dispatcher = Arc::clone(&dispatcher);
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::task::spawn_blocking(move || {
            barrier.wait();
            dispatcher.terminal_from_module(1)
        }));
    }
    barrier.wait();
    let first = tasks.remove(0).await.expect("terminal task must join");
    let second = tasks.remove(0).await.expect("terminal task must join");
    assert_ne!(first, second, "exactly one duplicate may release");
    assert_eq!(flow.in_flight(), 1);
    assert_eq!(flow.available_permits(), 1);
    assert_eq!(
        dispatcher
            .inbox
            .lock()
            .expect("route inbox mutex poisoned")
            .state(2),
        Some(SlotState::Delivered)
    );

    assert!(dispatcher.terminal_from_module(2));
    assert_eq!(flow.in_flight(), 0);
    assert_eq!(flow.available_permits(), 2);
}

#[tokio::test]
async fn binding_back_reference_and_registry_make_terminal_and_teardown_reachable() {
    let (dispatcher, _drain, flow, _module_rx, _synthetic_rx) = dispatcher_fixture(1, 2);
    prepare_delivered(&dispatcher, &flow, 9).await;
    let binding = SpikeRouteBinding::new(&dispatcher);
    assert!(binding.on_terminal(9));
    assert!(!binding.on_terminal(9));
    assert_eq!(flow.in_flight(), 0);

    let (other, _other_drain, _other_flow, _module_rx, _synthetic_rx) = dispatcher_fixture(1, 1);
    let registry = SpikeForwardingInner::default();
    registry.insert(
        SpikeRouteKey {
            connection: 100,
            channel: 7,
        },
        Arc::clone(&dispatcher),
    );
    registry.insert(
        SpikeRouteKey {
            connection: 200,
            channel: 8,
        },
        Arc::clone(&other),
    );
    assert_eq!(
        registry.begin_connection_teardown(100, TeardownKind::ConnectionClose),
        1
    );
    assert_eq!(
        dispatcher.push_request(10, request(10, b"late")),
        PushOutcome::Rejected(Admission::Closing)
    );
    assert!(dispatcher.finish_closed().is_empty());
    assert_eq!(
        other.push_request(11, request(11, b"still-open")),
        PushOutcome::Admitted
    );
}

async fn delivered_cancel_order(use_broken_second_writer: bool) -> Vec<FrameType> {
    let flow = Arc::new(ChannelFlow::new(1));
    let (module_tx, mut module_rx) = mpsc::channel(4);
    let broken_writer = FrameSink::new(module_tx.clone());
    let (synthetic_tx, _synthetic_rx) = mpsc::unbounded_channel();
    let (dispatcher, drain) = RouteDispatcher::new(
        1,
        Arc::clone(&flow),
        FrameSink::new(module_tx),
        synthetic_tx,
    );
    let gate = SendGate::new();
    let drain = tokio::spawn(drain.with_send_gate(Arc::clone(&gate)).run());

    assert_eq!(
        dispatcher.push_request(21, request(21, b"delayed-send")),
        PushOutcome::Admitted
    );
    timeout(Duration::from_secs(1), gate.wait_until_parked())
        .await
        .expect("request send never reached the gate");
    assert_eq!(
        dispatcher
            .inbox
            .lock()
            .expect("route inbox mutex poisoned")
            .state(21),
        Some(SlotState::Delivered)
    );

    let mut observed = Vec::new();
    if use_broken_second_writer {
        broken_writer
            .send(cancel(21))
            .await
            .expect("broken writer should reach the open sink");
        observed.push(
            timeout(Duration::from_secs(1), module_rx.recv())
                .await
                .expect("broken cancel timed out")
                .expect("module sink closed")
                .header
                .ty,
        );
    } else {
        assert_eq!(
            dispatcher.submit_cancel(cancel(21)),
            CancelDecision::QueuedForDrain
        );
        assert!(
            module_rx.try_recv().is_err(),
            "the drain must not send CANCEL before its parked Request"
        );
    }

    gate.release();
    while observed.len() < 2 {
        observed.push(
            timeout(Duration::from_secs(1), module_rx.recv())
                .await
                .expect("ordered module frame timed out")
                .expect("module sink closed")
                .header
                .ty,
        );
    }

    assert!(dispatcher.terminal_from_module(21));
    assert_eq!(flow.in_flight(), 0);
    dispatcher.begin_closing(TeardownKind::None);
    timeout(Duration::from_secs(1), drain)
        .await
        .expect("drain did not stop")
        .expect("drain task panicked");
    observed
}

#[tokio::test]
async fn delivered_cancel_is_request_ordered_and_discriminates_a_second_writer() {
    assert_eq!(
        delivered_cancel_order(false).await,
        vec![FrameType::Request, FrameType::Cancel]
    );
    assert_eq!(
        delivered_cancel_order(true).await,
        vec![FrameType::Cancel, FrameType::Request],
        "a deliberately broken second writer must reproduce N2"
    );
}

/// Structural guard: module-bound writes stay centralized in one send site.
///
/// MUTATION-TESTING HAZARD, READ BEFORE MUTATING `mod.rs`. This assertion reads
/// the SOURCE TEXT of its sibling module, so it reacts to the FILE CHANGING
/// rather than to any BEHAVIOUR changing. Two consequences, each producing a
/// confident wrong answer in opposite directions:
///
/// - A mutation that touches that call site reddens THIS test, and a failure
///   COUNT alone then reads as "the mutation was caught" when nothing about the
///   behaviour was proved.
/// - A mutation that leaves the text intact leaves this test green regardless of
///   what it did to the semantics.
///
/// So when mutating `mod.rs`, READ WHICH TEST DIED rather than counting
/// failures: if this one is in the set, discount it and confirm a
/// behaviour-named test also reddened. The general class -- any mechanism keyed
/// on the file rather than the behaviour -- is in docs/hunting-loop-briefing.md.
#[test]
fn route_drain_has_the_only_module_sink_send_site() {
    let source = include_str!("mod.rs");
    assert_eq!(
        source.matches("self.module_sink.send(frame).await").count(),
        1,
        "module-bound writes must remain centralized in RouteDrain::send_to_module"
    );
}

#[tokio::test]
async fn delivered_cancel_preempts_later_blocked_acquire_and_breaks_r5() {
    let (dispatcher, drain, flow, mut module_rx, _synthetic_rx) = dispatcher_fixture(1, 2);
    let drain = tokio::spawn(drain.run());

    assert_eq!(
        dispatcher.push_request(31, request(31, b"A")),
        PushOutcome::Admitted
    );
    let first = timeout(Duration::from_secs(1), module_rx.recv())
        .await
        .expect("request A timed out")
        .expect("module sink closed");
    assert_eq!(
        (first.header.ty, first.header.corr),
        (FrameType::Request, 31)
    );
    assert_eq!(flow.in_flight(), 1);

    assert_eq!(
        dispatcher.push_request(32, request(32, b"B")),
        PushOutcome::Admitted
    );
    timeout(Duration::from_secs(1), async {
        loop {
            if dispatcher
                .inbox
                .lock()
                .expect("route inbox mutex poisoned")
                .state(32)
                == Some(SlotState::Sending { cancelled: false })
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("request B never blocked in Sending");

    assert_eq!(
        dispatcher.submit_cancel(cancel(31)),
        CancelDecision::QueuedForDrain
    );
    let forwarded_cancel = timeout(Duration::from_secs(1), module_rx.recv())
        .await
        .expect("CANCEL(A) was blocked behind B's acquire")
        .expect("module sink closed");
    assert_eq!(
        (forwarded_cancel.header.ty, forwarded_cancel.header.corr),
        (FrameType::Cancel, 31)
    );
    assert_eq!(flow.in_flight(), 1, "B must not acquire A's live credit");
    assert_eq!(
        dispatcher
            .inbox
            .lock()
            .expect("route inbox mutex poisoned")
            .state(32),
        Some(SlotState::Sending { cancelled: false })
    );
    assert!(module_rx.try_recv().is_err(), "B was sent before A settled");

    assert!(dispatcher.terminal_from_module(31));
    let second = timeout(Duration::from_secs(1), module_rx.recv())
        .await
        .expect("request B did not proceed after A settled")
        .expect("module sink closed");
    assert_eq!(
        (second.header.ty, second.header.corr),
        (FrameType::Request, 32)
    );
    assert!(dispatcher.terminal_from_module(32));
    assert_eq!(flow.in_flight(), 0);
    assert_eq!(flow.available_permits(), 1);

    dispatcher.begin_closing(TeardownKind::None);
    timeout(Duration::from_secs(1), drain)
        .await
        .expect("drain did not stop")
        .expect("drain task panicked");
}

#[tokio::test]
async fn async_drain_skips_queued_cancel_and_preserves_fifo_credit() {
    let (dispatcher, drain, flow, mut module_rx, mut synthetic_rx) = dispatcher_fixture(1, 3);
    for corr in 1..=3 {
        assert_eq!(
            dispatcher.push_request(corr, request(corr, &[corr as u8])),
            PushOutcome::Admitted
        );
    }
    assert_eq!(
        dispatcher.submit_cancel(cancel(2)),
        CancelDecision::SynthesizeCancelled
    );

    let drain = tokio::spawn(drain.run());
    let first = timeout(Duration::from_secs(1), module_rx.recv())
        .await
        .expect("first dispatch timed out")
        .expect("module sink closed");
    assert_eq!(first.header.corr, 1);
    assert!(module_rx.try_recv().is_err(), "window=1 must block corr 3");
    assert!(dispatcher.terminal_from_module(1));

    let third = timeout(Duration::from_secs(1), module_rx.recv())
        .await
        .expect("third dispatch timed out")
        .expect("module sink closed");
    assert_eq!(third.header.corr, 3);
    assert!(dispatcher.terminal_from_module(3));

    let synthetic = synthetic_rx.recv().await.expect("cancel terminal missing");
    assert_eq!(
        synthetic,
        SyntheticTerminal {
            corr: 2,
            kind: SyntheticKind::Cancelled,
        }
    );
    assert!(
        module_rx.try_recv().is_err(),
        "cancelled corr reached module"
    );
    assert_eq!(flow.in_flight(), 0);
    assert_eq!(flow.available_permits(), 1);

    dispatcher.begin_closing(TeardownKind::None);
    timeout(Duration::from_secs(1), drain)
        .await
        .expect("drain did not stop")
        .expect("drain task panicked");
}

#[tokio::test]
async fn cancel_during_blocked_acquire_is_rolled_back_before_send() {
    let (dispatcher, drain, flow, mut module_rx, mut synthetic_rx) = dispatcher_fixture(1, 1);
    flow.acquire().await.expect("window is open");
    assert_eq!(
        dispatcher.push_request(4, request(4, b"blocked-acquire")),
        PushOutcome::Admitted
    );
    let drain = tokio::spawn(drain.run());

    timeout(Duration::from_secs(1), async {
        loop {
            if dispatcher
                .inbox
                .lock()
                .expect("route inbox mutex poisoned")
                .state(4)
                == Some(SlotState::Sending { cancelled: false })
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("drain never entered Sending");
    assert_eq!(
        dispatcher.submit_cancel(cancel(4)),
        CancelDecision::DeferredToDrain
    );

    flow.release();
    assert_eq!(
        timeout(Duration::from_secs(1), synthetic_rx.recv())
            .await
            .expect("cancel terminal timed out")
            .expect("synthetic sink closed"),
        SyntheticTerminal {
            corr: 4,
            kind: SyntheticKind::Cancelled,
        }
    );
    assert!(module_rx.try_recv().is_err());
    assert_eq!(flow.in_flight(), 0);
    assert_eq!(flow.available_permits(), 1);

    dispatcher.begin_closing(TeardownKind::None);
    timeout(Duration::from_secs(1), drain)
        .await
        .expect("drain did not stop")
        .expect("drain task panicked");
}

#[tokio::test]
async fn async_drain_send_failure_rolls_back_delivered_credit() {
    let (dispatcher, drain, flow, module_rx, mut synthetic_rx) = dispatcher_fixture(1, 1);
    drop(module_rx);
    assert_eq!(
        dispatcher.push_request(5, request(5, b"send-fails")),
        PushOutcome::Admitted
    );
    let drain = tokio::spawn(drain.run());
    assert_eq!(
        timeout(Duration::from_secs(1), synthetic_rx.recv())
            .await
            .expect("backend terminal timed out")
            .expect("synthetic sink closed"),
        SyntheticTerminal {
            corr: 5,
            kind: SyntheticKind::BackendError,
        }
    );
    assert_eq!(flow.in_flight(), 0);
    assert_eq!(flow.available_permits(), 1);
    dispatcher.begin_closing(TeardownKind::None);
    timeout(Duration::from_secs(1), drain)
        .await
        .expect("drain did not stop")
        .expect("drain task panicked");
}

async fn closed_flow_result(kind: TeardownKind) -> Option<SyntheticTerminal> {
    let (dispatcher, drain, flow, _module_rx, mut synthetic_rx) = dispatcher_fixture(1, 1);
    assert_eq!(
        dispatcher.push_request(13, request(13, b"blocked")),
        PushOutcome::Admitted
    );
    dispatcher.close_flow(kind);
    drain.run().await;
    assert_eq!(flow.in_flight(), 0);
    synthetic_rx.try_recv().ok()
}

#[tokio::test]
async fn closed_flow_is_disambiguated_by_teardown_kind() {
    assert_eq!(
        closed_flow_result(TeardownKind::Reloading).await,
        Some(SyntheticTerminal {
            corr: 13,
            kind: SyntheticKind::ModuleReloading,
        })
    );
    assert_eq!(closed_flow_result(TeardownKind::Goodbye).await, None);
    assert_eq!(
        closed_flow_result(TeardownKind::ConnectionClose).await,
        None
    );
    assert_eq!(
        closed_flow_result(TeardownKind::None).await,
        Some(SyntheticTerminal {
            corr: 13,
            kind: SyntheticKind::BackendError,
        })
    );
}

#[cfg(loom)]
mod loom_model {
    use loom::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
        thread,
    };

    use super::*;

    struct ModelFlow {
        window: usize,
        available: AtomicUsize,
        in_flight: AtomicUsize,
        releases: AtomicUsize,
    }

    impl ModelFlow {
        fn new(window: usize) -> Self {
            Self {
                window,
                available: AtomicUsize::new(window),
                in_flight: AtomicUsize::new(0),
                releases: AtomicUsize::new(0),
            }
        }

        fn acquire(&self) {
            let available = self.available.fetch_sub(1, Ordering::AcqRel);
            assert!(available > 0, "model acquired beyond its window");
            self.in_flight.fetch_add(1, Ordering::AcqRel);
        }
    }

    impl CreditRelease for Arc<ModelFlow> {
        fn release_credit(&self) {
            let mut observed = self.in_flight.load(Ordering::Acquire);
            loop {
                if observed == 0 {
                    return;
                }
                match self.in_flight.compare_exchange_weak(
                    observed,
                    observed - 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        self.available.fetch_add(1, Ordering::AcqRel);
                        self.releases.fetch_add(1, Ordering::AcqRel);
                        return;
                    }
                    Err(next) => observed = next,
                }
            }
        }
    }

    #[test]
    fn two_requests_and_duplicate_terminal_are_model_checked() {
        loom::model(|| {
            let flow = Arc::new(ModelFlow::new(2));
            let inbox = Arc::new(Mutex::new(RouteInbox::new(2)));

            for corr in 1..=2 {
                let mut locked = inbox.lock().expect("loom inbox mutex poisoned");
                assert_eq!(
                    locked.push_request(corr, request(corr, &[corr as u8])),
                    PushOutcome::Admitted
                );
                assert_eq!(locked.pop_for_dispatch(), Some(corr));
                drop(locked);

                flow.acquire();
                let token = CreditToken::new(Arc::clone(&flow));
                assert!(matches!(
                    inbox
                        .lock()
                        .expect("loom inbox mutex poisoned")
                        .commit_delivered(corr),
                    CommitDecision::Proceed(_)
                ));
                token.commit();
            }

            let mut duplicates = Vec::new();
            for _ in 0..2 {
                let flow = Arc::clone(&flow);
                let inbox = Arc::clone(&inbox);
                duplicates.push(thread::spawn(move || {
                    if inbox
                        .lock()
                        .expect("loom inbox mutex poisoned")
                        .on_terminal(1)
                    {
                        flow.release_credit();
                        true
                    } else {
                        false
                    }
                }));
            }

            let first = duplicates.remove(0).join().expect("loom thread panicked");
            let second = duplicates.remove(0).join().expect("loom thread panicked");
            assert_ne!(first, second);
            assert_eq!(flow.in_flight.load(Ordering::Acquire), 1);
            assert_eq!(flow.available.load(Ordering::Acquire), 1);
            assert_eq!(flow.releases.load(Ordering::Acquire), 1);
            assert_eq!(flow.window, 2);

            if inbox
                .lock()
                .expect("loom inbox mutex poisoned")
                .on_terminal(2)
            {
                flow.release_credit();
            }
            assert_eq!(flow.in_flight.load(Ordering::Acquire), 0);
            assert_eq!(flow.available.load(Ordering::Acquire), 2);
            assert_eq!(flow.releases.load(Ordering::Acquire), 2);
        });
    }
}
