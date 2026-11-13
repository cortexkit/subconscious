//! The first-party mark as the configuration gate decides it: activation
//! after the spawn acknowledged its module's active generation and reported
//! ready, and lowering. These tests run without a forwarding table, so the
//! mark lives on the spawn's authority record; the control tests cover what
//! activation and lowering publish.

use super::*;
use crate::spawn_authority::{MarkStatus, SpawnAuthorityRecord, INITIAL_AUTHORITY_EPOCH};
use subc_protocol::peer::CallerOrigin;

const RESTARTED: ConnectionId = ConnectionId::new(30);

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
