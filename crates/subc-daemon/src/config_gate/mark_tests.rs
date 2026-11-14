//! The first-party mark as the configuration gate decides it: activation
//! after the spawn acknowledged its module's active generation and reported
//! ready, and lowering. These tests run without a forwarding table, so the
//! mark lives on the spawn's authority record; the control tests cover what
//! activation and lowering publish.

use super::*;
use crate::spawn_authority::{MarkStatus, SpawnAuthorityRecord, INITIAL_AUTHORITY_EPOCH};
use subc_protocol::peer::CallerOrigin;

const RESTARTED: ConnectionId = ConnectionId::new(30);

/// Makes a connection the active delivery target for a spawn and
/// acknowledges the active generation for it, as the spawn's SDK would.
#[cfg(unix)]
pub(crate) type AcknowledgeActive = Box<dyn Fn(ConnectionId, &DeliverySpawn)>;

#[cfg(unix)]
impl RuntimeOwner {
    /// A supervised, stamped gate whose module `a` has an active generation
    /// and no pending change, with no running incumbent: the setting of a
    /// crash restart. For supervisor tests of the first-party mark.
    pub(crate) fn mark_supervisor_fixture_for_test() -> (
        impl Drop,
        std::sync::Arc<Self>,
        crate::supervise::Supervisor,
        crate::supervise::ModuleSpec,
        AcknowledgeActive,
    ) {
        let (rig, owner, supervisor, _handler) = supervised_fixture_with_carry(false, false);
        applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
        owner.disconnected(INCUMBENT);
        let path = rig.path.join("subc.jsonc");
        let (config, _) = crate::daemon_config::parse_with_activity(
            &std::fs::read_to_string(&path).unwrap(),
            &path,
        )
        .unwrap();
        let spec = config
            .modules
            .into_iter()
            .find(|entry| entry.module_id == "a")
            .unwrap()
            .module_spec();
        let declaration = rig.declaration.clone();
        let acknowledging = owner.clone();
        let acknowledge: AcknowledgeActive =
            Box::new(move |connection: ConnectionId, spawn: &DeliverySpawn| {
                acknowledging
                    .declare(connection, "a", declaration.clone(), true)
                    .unwrap();
                acknowledging.registered(connection, spawn).unwrap();
                let view = pending(&acknowledging, connection);
                assert_eq!(
                    applied(&acknowledging, connection, &view, true).unwrap(),
                    AppliedDisposition::CatchupComplete
                );
            });
        (rig, owner, supervisor, spec, acknowledge)
    }
}

fn released(spawn: u64) -> std::sync::Arc<SpawnAuthorityRecord> {
    let record = SpawnAuthorityRecord::new("incarnation", spawn, "a");
    record.set_origin_for_test(CallerOrigin::Release);
    record
}

/// A spawn of module `a` launched now, carrying `record`, admitted under the
/// stamp when `stamped`.
fn launch(
    owner: &RuntimeOwner,
    record: &std::sync::Arc<SpawnAuthorityRecord>,
    stamped: bool,
) -> DeliverySpawn {
    owner
        .spawned(
            "a",
            tokio::time::Instant::now(),
            Duration::from_secs(120),
            Duration::from_secs(5),
        )
        .unwrap()
        .unwrap()
        .with_authority_for_test(record.clone(), stamped)
}

/// Register `spawn` as the module's active delivery target on `connection`,
/// as the supervisor does after its HELLO.
fn register(rig: &Rig, owner: &RuntimeOwner, connection: ConnectionId, spawn: &DeliverySpawn) {
    rig.declare(owner, connection.get());
    owner.registered(connection, spawn).unwrap();
}

/// An approved generation 1, active for the stamp-running incumbent, whose
/// connection then ends as a crashed child's does.
fn active_then_incumbent_gone() -> (Rig, RuntimeOwner) {
    let (rig, owner) = rig();
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    owner.disconnected(INCUMBENT);
    (rig, owner)
}

fn ack(owner: &RuntimeOwner, connection: ConnectionId) -> AppliedDisposition {
    let view = pending(owner, connection);
    assert_eq!(
        view.generation, 1,
        "the catch-up names the active generation"
    );
    applied(owner, connection, &view, true).unwrap()
}

fn assert_unmarked(record: &SpawnAuthorityRecord, status: MarkStatus) {
    assert!(!record.first_party());
    assert_eq!(record.mark_status(), status);
}

#[test]
fn first_party_mark_restart_activates_after_acknowledgement_then_readiness() {
    let (rig, owner) = active_then_incumbent_gone();
    let record = released(2);
    let spawn = launch(&owner, &record, true);
    register(&rig, &owner, RESTARTED, &spawn);
    assert_eq!(ack(&owner, RESTARTED), AppliedDisposition::CatchupComplete);
    assert_unmarked(&record, MarkStatus::AwaitingReadiness);
    owner.mark_ready(&spawn).unwrap();
    assert!(record.first_party());
    assert_eq!(record.mark_status(), MarkStatus::Marked);
    // Activation is not an authority change of the epoch kind.
    assert_eq!(record.epoch_for_test(), INITIAL_AUTHORITY_EPOCH);
}

#[test]
fn first_party_mark_restart_activates_after_readiness_then_acknowledgement() {
    let (rig, owner) = active_then_incumbent_gone();
    let record = released(2);
    let spawn = launch(&owner, &record, true);
    register(&rig, &owner, RESTARTED, &spawn);
    owner.mark_ready(&spawn).unwrap();
    // Readiness and the delivered active view are not enough on their own.
    assert_unmarked(&record, MarkStatus::AwaitingAcknowledgement);
    assert_eq!(ack(&owner, RESTARTED), AppliedDisposition::CatchupComplete);
    assert!(record.first_party());
    assert_eq!(record.epoch_for_test(), INITIAL_AUTHORITY_EPOCH);
    // The acknowledgement is asked for once: a later read serves the active
    // view alone.
    let reply = owner
        .module_get(RESTARTED, "a", ConfigGetRequest::new())
        .unwrap();
    assert!(reply.pending.is_none());
}

#[test]
fn first_party_mark_boot_restart_activates_the_same_way() {
    let (rig, owner) = active_then_incumbent_gone();
    drop(owner);
    let booted = rig.owner("next-daemon");
    let record = released(1);
    let spawn = launch(&booted, &record, true);
    register(&rig, &booted, RESTARTED, &spawn);
    booted.mark_ready(&spawn).unwrap();
    assert_unmarked(&record, MarkStatus::AwaitingAcknowledgement);
    assert_eq!(ack(&booted, RESTARTED), AppliedDisposition::CatchupComplete);
    assert!(record.first_party());
}

#[test]
fn first_party_mark_refused_acknowledgement_leaves_the_spawn_unmarked_without_recovery() {
    let (rig, owner) = active_then_incumbent_gone();
    let record = released(2);
    let spawn = launch(&owner, &record, true);
    register(&rig, &owner, RESTARTED, &spawn);
    owner.mark_ready(&spawn).unwrap();
    let view = pending(&owner, RESTARTED);
    assert_eq!(
        applied(&owner, RESTARTED, &view, false).unwrap(),
        AppliedDisposition::Ignored,
        "a refused mark-only acknowledgement is not a delivery failure"
    );
    assert!(owner.delivery_take_recoveries().unwrap().is_empty());
    assert_unmarked(&record, MarkStatus::AcknowledgementRefused);
    owner.mark_ready(&spawn).unwrap();
    assert_unmarked(&record, MarkStatus::AcknowledgementRefused);
    assert_active(&owner, RESTARTED, 1);
}

#[test]
fn first_party_mark_acknowledgement_counts_only_for_its_own_spawn_and_generation() {
    let (rig, owner) = active_then_incumbent_gone();
    let earlier = released(2);
    let first = launch(&owner, &earlier, true);
    register(&rig, &owner, RESTARTED, &first);
    let view = pending(&owner, RESTARTED);
    // An acknowledgement naming another generation is ignored.
    let mut wrong = view.clone();
    wrong.generation += 1;
    assert_eq!(
        applied(&owner, RESTARTED, &wrong, true).unwrap(),
        AppliedDisposition::Ignored
    );
    owner.mark_ready(&first).unwrap();
    assert_unmarked(&earlier, MarkStatus::AwaitingAcknowledgement);
    // A later spawn now registered on the same connection never inherits
    // the earlier spawn's assigned acknowledgement.
    let later = released(3);
    let second = launch(&owner, &later, true);
    owner.registered(RESTARTED, &second).unwrap();
    owner.mark_ready(&second).unwrap();
    assert_eq!(
        applied(&owner, RESTARTED, &view, true).unwrap(),
        AppliedDisposition::CatchupComplete
    );
    assert!(
        earlier.first_party(),
        "the acknowledgement was the earlier spawn's"
    );
    assert_unmarked(&later, MarkStatus::AwaitingAcknowledgement);
}

#[test]
fn first_party_mark_never_asks_an_unstamped_or_ungated_spawn_to_acknowledge() {
    // Admitted without the stamp's gate: the restarted child's delivery is
    // exactly what it was, with no catch-up attempt.
    let (rig, owner) = active_then_incumbent_gone();
    let record = released(2);
    let spawn = launch(&owner, &record, false);
    register(&rig, &owner, RESTARTED, &spawn);
    let reply = owner
        .module_get(RESTARTED, "a", ConfigGetRequest::new())
        .unwrap();
    assert!(reply.pending.is_none(), "{reply:?}");
    assert!(owner.delivery_window(RESTARTED).unwrap().is_none());
    owner.mark_ready(&spawn).unwrap();
    assert_unmarked(&record, MarkStatus::NotEligible);

    // An unstamped install: nothing is asked and nothing is marked.
    let rig = Rig::new();
    write(&rig, 1);
    let owner = rig.owner("unstamped");
    let record = released(1);
    let spawn = launch(&owner, &record, false);
    register(&rig, &owner, RESTARTED, &spawn);
    let reply = owner
        .module_get(RESTARTED, "a", ConfigGetRequest::new())
        .unwrap();
    assert!(reply.pending.is_none(), "{reply:?}");
    assert!(owner.delivery_window(RESTARTED).unwrap().is_none());
    owner.mark_ready(&spawn).unwrap();
    assert!(!record.first_party());
}

#[test]
fn first_party_mark_stamp_never_marks_an_already_running_child() {
    // The incumbent ran before the stamp; it acknowledges the first approved
    // generation and reports ready, and still is not marked.
    let rig = Rig::new();
    write(&rig, 1);
    let owner = rig.owner("running-before-stamp");
    let record = released(1);
    let spawn = launch(&owner, &record, false);
    register(&rig, &owner, INCUMBENT, &spawn);
    rig.stamp(&owner);
    assert_eq!(
        applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap(),
        AppliedDisposition::Active
    );
    owner.mark_ready(&spawn).unwrap();
    assert_unmarked(&record, MarkStatus::NotEligible);
}

#[test]
fn first_party_mark_module_blocking_readiness_is_never_marked_and_is_reported() {
    let (rig, owner) = active_then_incumbent_gone();
    let record = released(2);
    let spawn = launch(&owner, &record, true);
    register(&rig, &owner, RESTARTED, &spawn);
    assert_eq!(ack(&owner, RESTARTED), AppliedDisposition::CatchupComplete);
    // The module keeps answering "not ready" while it waits for a grant that
    // only the mark would give it.
    for _ in 0..3 {
        owner.mark_not_ready(&spawn).unwrap();
        assert_unmarked(&record, MarkStatus::AwaitingReadiness);
    }
}

#[test]
fn first_party_mark_early_promoted_candidate_waits_for_its_own_generation() {
    let (rig, owner) = rig();
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    approve_kinds(&rig, &owner, &[subc_config::ApplyKind::ModuleRestart]);
    let record = released(2);
    let spawn = launch(&owner, &record, true);
    candidate(&rig, &owner);
    owner.registered(CANDIDATE, &spawn).unwrap();
    owner.mark_ready(&spawn).unwrap();
    // The incumbent died, so the candidate is promoted before its warm-up
    // finished. It is ready and the active target, but generation 2 is not
    // active yet.
    owner.disconnected(INCUMBENT);
    owner
        .promoted(CANDIDATE, "a", tokio::time::Instant::now())
        .unwrap();
    owner.mark_ready(&spawn).unwrap();
    assert_unmarked(&record, MarkStatus::AwaitingAcknowledgement);
    let view = pending(&owner, CANDIDATE);
    assert_eq!(view.generation, 2);
    owner.ready(CANDIDATE, "a", 2).unwrap();
    assert!(!record.first_party());
    assert_eq!(
        applied(&owner, CANDIDATE, &view, true).unwrap(),
        AppliedDisposition::Active
    );
    assert!(record.first_party(), "{:?}", record.mark_status());
}

#[test]
fn first_party_mark_development_placement_requires_explicit_non_hosting() {
    for hosts_third_party_code in [Some(true), None, Some(false)] {
        let (rig, owner) = active_then_incumbent_gone();
        let record = SpawnAuthorityRecord::new("incarnation", 2, "a");
        record.initialize_origin(
            CallerOrigin::Development,
            Default::default(),
            hosts_third_party_code,
        );
        record.set_development_daemon_for_test(true);
        let spawn = launch(&owner, &record, true);
        register(&rig, &owner, RESTARTED, &spawn);
        owner.mark_ready(&spawn).unwrap();
        let reply = owner
            .module_get(RESTARTED, "a", ConfigGetRequest::new())
            .unwrap();
        if reply.pending.is_some() {
            assert_eq!(ack(&owner, RESTARTED), AppliedDisposition::CatchupComplete);
        }
        assert_eq!(
            record.first_party(),
            hosts_third_party_code == Some(false),
            "hosts_third_party_code: {hosts_third_party_code:?}"
        );
        assert_eq!(record.capture().unwrap().origin, CallerOrigin::Development);
        if hosts_third_party_code != Some(false) {
            assert!(reply.pending.is_none());
            assert_unmarked(&record, MarkStatus::NotEligible);
        }
    }
}

#[test]
fn first_party_mark_development_origin_needs_a_development_daemon() {
    for (development, marked) in [(true, true), (false, false)] {
        let (rig, owner) = active_then_incumbent_gone();
        let record = SpawnAuthorityRecord::new("incarnation", 2, "a");
        record.set_origin_for_test(CallerOrigin::Development);
        record.set_development_daemon_for_test(development);
        let spawn = launch(&owner, &record, true);
        register(&rig, &owner, RESTARTED, &spawn);
        owner.mark_ready(&spawn).unwrap();
        if marked {
            assert_eq!(ack(&owner, RESTARTED), AppliedDisposition::CatchupComplete);
        } else {
            // A release daemon does not even ask for the acknowledgement.
            let reply = owner
                .module_get(RESTARTED, "a", ConfigGetRequest::new())
                .unwrap();
            assert!(reply.pending.is_none());
        }
        assert_eq!(
            record.first_party(),
            marked,
            "development daemon: {development}"
        );
    }
}

#[test]
fn first_party_mark_unstamp_lowers_and_bumps_the_epoch() {
    let (rig, owner) = active_then_incumbent_gone();
    let record = released(2);
    let spawn = launch(&owner, &record, true);
    register(&rig, &owner, RESTARTED, &spawn);
    owner.mark_ready(&spawn).unwrap();
    ack(&owner, RESTARTED);
    assert!(record.first_party());
    let plan = owner
        .finish_plan(owner.prepare(PlanKind::Unstamp).unwrap(), daemon_review)
        .unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let challenge = owner.begin_approval(&plan.digest, &Rig::facts()).unwrap();
    owner
        .finish_approval(
            challenge.attempt(),
            &Rig::facts(),
            ApprovalReply::Presence(
                super::super::super::super::PresenceDecision::InteractiveApproval,
            ),
            2,
        )
        .unwrap();
    assert_unmarked_lowered(&record);
    // Lowering is final, even if the spawn reports ready again.
    owner.mark_ready(&spawn).unwrap();
    assert_unmarked_lowered(&record);
}

fn assert_unmarked_lowered(record: &SpawnAuthorityRecord) {
    assert!(!record.first_party());
    assert_eq!(record.mark_status(), MarkStatus::Lowered);
    assert_eq!(record.epoch_for_test(), INITIAL_AUTHORITY_EPOCH + 1);
}

#[test]
fn first_party_mark_failed_delivery_lowers_the_spawn() {
    let (rig, owner) = active_then_incumbent_gone();
    let record = released(2);
    let spawn = launch(&owner, &record, true);
    register(&rig, &owner, RESTARTED, &spawn);
    owner.mark_ready(&spawn).unwrap();
    ack(&owner, RESTARTED);
    assert!(record.first_party());
    approve(&rig, &owner, 2);
    let view = pending(&owner, RESTARTED);
    assert_eq!(view.generation, 2);
    assert!(matches!(
        applied(&owner, RESTARTED, &view, false).unwrap(),
        AppliedDisposition::Failed { .. }
    ));
    assert_unmarked_lowered(&record);
}

#[test]
fn first_party_mark_failed_delivery_rules_out_a_spawn_not_yet_marked() {
    let (rig, owner) = active_then_incumbent_gone();
    let record = released(2);
    let spawn = launch(&owner, &record, true);
    register(&rig, &owner, RESTARTED, &spawn);
    // The spawn acknowledged the active generation but has not reported
    // ready when its delivery of the next one fails.
    assert_eq!(ack(&owner, RESTARTED), AppliedDisposition::CatchupComplete);
    assert_unmarked(&record, MarkStatus::AwaitingReadiness);
    approve(&rig, &owner, 2);
    let view = pending(&owner, RESTARTED);
    assert_eq!(view.generation, 2);
    assert!(matches!(
        applied(&owner, RESTARTED, &view, false).unwrap(),
        AppliedDisposition::Failed { .. }
    ));
    assert_unmarked(&record, MarkStatus::Lowered);
    // Reporting ready afterwards never activates it, and nothing it
    // attested changed, so its epoch did not move.
    owner.mark_ready(&spawn).unwrap();
    assert_unmarked(&record, MarkStatus::Lowered);
    assert_eq!(record.epoch_for_test(), INITIAL_AUTHORITY_EPOCH);
}

#[tokio::test]
async fn first_party_mark_warm_up_probe_never_consumes_the_mark_acknowledgement() {
    // Generation 2 rebinds routes, so its route policy is installed and the
    // ordinary delivery waits on route readiness; it becomes active.
    let (rig, owner) = rig();
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    let owner = std::sync::Arc::new(owner);
    let forwarding = std::sync::Arc::new(crate::ForwardingTable::default());
    let (module_tx, _module_rx) = tokio::sync::mpsc::channel(32);
    forwarding
        .register_module_connection(
            INCUMBENT,
            "a".into(),
            subc_protocol::PROTOCOL_VERSION,
            subc_protocol::manifest::Concurrency::ModuleManaged,
            crate::FrameSink::new(module_tx),
        )
        .unwrap();
    let _handler = crate::ControlHandler::with_forwarding(
        std::sync::Arc::new(crate::Registry::default()),
        forwarding,
    )
    .with_config_runtime(owner.clone());
    approve_kinds(&rig, &owner, &[subc_config::ApplyKind::RouteRebind]);
    let view = pending(&owner, INCUMBENT);
    assert_eq!(
        applied(&owner, INCUMBENT, &view, true).unwrap(),
        AppliedDisposition::WaitingForReadiness
    );
    owner
        .install_approved_route_policy("a", view.generation)
        .await
        .expect("the acknowledged route policy installs");
    owner.ready(INCUMBENT, "a", view.generation).unwrap();
    assert_active(&owner, INCUMBENT, 2);
    owner.disconnected(INCUMBENT);

    // A restarted spawn is asked to acknowledge the active generation for
    // its mark, and a warm-up health probe answers not ready first.
    let record = released(3);
    let spawn = launch(&owner, &record, true);
    register(&rig, &owner, RESTARTED, &spawn);
    let catchup = pending(&owner, RESTARTED);
    assert_eq!(catchup.generation, 2);
    let probe = owner
        .delivery_health_probe(RESTARTED, "a")
        .unwrap()
        .expect("the restarted spawn is probed");
    assert_eq!(owner.delivery_health_result(probe, false).unwrap(), None);
    assert_unmarked(&record, MarkStatus::AwaitingAcknowledgement);
    // Nor is a route-policy probe due for an attempt that only asks for the
    // acknowledgement.
    assert_eq!(owner.policy_probe_due(RESTARTED).unwrap(), None);

    // Its own acknowledgement still settles the attempt and earns the mark.
    assert_eq!(
        applied(&owner, RESTARTED, &catchup, true).unwrap(),
        AppliedDisposition::CatchupComplete
    );
    owner.mark_ready(&spawn).unwrap();
    assert!(record.first_party(), "{:?}", record.mark_status());
}
