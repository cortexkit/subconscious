use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use subc_protocol::scope::{ScopeAttributes, ScopeCarrier, ScopeKind};

const OWNER: &str = "owner";
const OTHER: &str = "other";

fn clocked_table() -> (ScopeTable, Arc<AtomicU64>) {
    let clock = Arc::new(AtomicU64::new(1_000));
    let source = Arc::clone(&clock);
    let mut table = ScopeTable::new([OWNER.to_string()]);
    table.set_wall_clock(Arc::new(move || source.load(Ordering::SeqCst)));
    (table, clock)
}

fn record(scope_ref: &str, epoch: u64) -> ScopeRecord {
    ScopeRecord::new(scope_ref, epoch, ScopeKind::Ephemeral)
}

fn sync(
    table: &mut ScopeTable,
    owner: &str,
    generation: u64,
    records: Vec<ScopeRecord>,
) -> SyncApplied {
    let records = crate::control::ControlHandler::prepare_scope_records(records);
    table
        .sync(owner, ConnectionId::new(1), |_| true, generation, records)
        .unwrap()
}

fn apply(
    table: &mut ScopeTable,
    generation: u64,
    records: Vec<ScopeRecord>,
    end: Vec<ScopeEnd>,
) -> SyncApplied {
    let records = crate::control::ControlHandler::prepare_scope_records(records);
    table
        .apply(
            OWNER,
            ConnectionId::new(1),
            |_| true,
            generation,
            records,
            end,
        )
        .unwrap()
}

fn status(table: &ScopeTable, owner: &str, scope_ref: &str) -> ScopeStatus {
    table.describe(&reserved(owner), scope_ref).status
}

fn code(applied: &SyncApplied) -> Option<&str> {
    applied.results[0].code.as_deref()
}

#[test]
fn sweep_expiry_uses_wall_time_and_refreshes_child_links() {
    let (mut table, clock) = clocked_table();
    let parent = record("parent", 7)
        .with_expires_at_ms(Some(2_000))
        .with_child_owners(vec![reserved(OTHER)]);
    sync(&mut table, OWNER, 10, vec![parent, record("forever", 1)]);
    sync(
        &mut table,
        OTHER,
        1,
        vec![record("child", 1).with_parent(Some(ScopeParent::new(reserved(OWNER), "parent", 7)))],
    );
    clock.store(500, Ordering::SeqCst);
    assert!(table.sweep_expired().expired.is_empty());
    clock.store(2_000, Ordering::SeqCst);
    let swept = table.sweep_expired();
    assert_eq!(swept.expired.len(), 1);
    assert_eq!(swept.expired[0].scope_epoch, 7);
    assert_eq!(swept.expired[0].expires_at_ms, 2_000);
    let ended = table.describe(&reserved(OWNER), "parent");
    assert_eq!(
        (ended.status, ended.scope_epoch),
        (ScopeStatus::Ended, Some(7))
    );
    assert_eq!(status(&table, OWNER, "forever"), ScopeStatus::Live);
    let child = table.describe(&reserved(OTHER), "child");
    assert_eq!(child.status, ScopeStatus::Live);
    assert_eq!(child.stamp.unwrap().parent_state, Some(ParentState::Ended));
    assert!(swept
        .tag_changes
        .iter()
        .any(|change| change.scope_ref == "parent"
            && change.drain == ScopeDrain::All(RouteCloseReason::ScopeEnded)));
    assert!(swept
        .tag_changes
        .iter()
        .any(|change| change.scope_ref == "child"
            && change.drain == ScopeDrain::All(RouteCloseReason::ScopeParentEnded)));
    assert_eq!(table.owners[OWNER].authority.unwrap().last_generation, 10);
    assert_eq!(
        table
            .sync(OWNER, ConnectionId::new(1), |_| true, 10, vec![])
            .unwrap_err()
            .code,
        error_codes::SCOPE_SYNC_STALE
    );
    clock.store(100, Ordering::SeqCst);
    let resent = sync(
        &mut table,
        OWNER,
        11,
        vec![record("parent", 7), record("forever", 1)],
    );
    assert_eq!(code(&resent), Some(error_codes::SCOPE_EXPIRED));
    assert!(resent.ended.is_empty());
}

/// The expiry loop ticks only while this is true, so it must turn on with the
/// first deadline and off once the last deadline-bearing scope has ended.
#[test]
fn has_deadlines_tracks_only_live_scopes_with_a_deadline() {
    let (mut table, clock) = clocked_table();
    assert!(!table.has_deadlines());
    sync(&mut table, OWNER, 1, vec![record("plain", 1)]);
    assert!(!table.has_deadlines());
    sync(
        &mut table,
        OWNER,
        2,
        vec![
            record("plain", 1),
            record("run", 1).with_expires_at_ms(Some(2_000)),
        ],
    );
    assert!(table.has_deadlines());
    clock.store(2_000, Ordering::SeqCst);
    table.sweep_expired();
    assert!(!table.has_deadlines());
}

#[test]
fn sweep_expiry_outlives_authority_connection() {
    let (mut table, clock) = clocked_table();
    let scope = record("s", 1).with_expires_at_ms(Some(2_000));
    sync(&mut table, OWNER, 50, vec![scope.clone()]);
    table.release_connection(ConnectionId::new(1));
    clock.store(2_001, Ordering::SeqCst);
    assert_eq!(table.sweep_expired().expired.len(), 1);
    assert!(table.owners[OWNER].authority.is_none());
    let reply = table
        .sync(OWNER, ConnectionId::new(2), |_| true, 0, vec![scope])
        .unwrap();
    assert_eq!(code(&reply), Some(error_codes::SCOPE_EXPIRED));
    assert!(reply.ended.is_empty());
    assert_eq!(
        table.owners[OWNER].authority.unwrap().connection_id,
        ConnectionId::new(2)
    );
}

#[test]
fn accepted_sync_and_apply_sweep_only_calling_owner() {
    for delta in [false, true] {
        let (mut table, clock) = clocked_table();
        let scope = record("s", 1).with_expires_at_ms(Some(2_000));
        sync(&mut table, OWNER, 1, vec![scope.clone()]);
        sync(&mut table, OTHER, 1, vec![scope.clone()]);
        clock.store(2_000, Ordering::SeqCst);
        let reply = if delta {
            apply(&mut table, 2, vec![scope], vec![])
        } else {
            sync(&mut table, OWNER, 2, vec![scope])
        };
        assert_eq!(code(&reply), Some(error_codes::SCOPE_EXPIRED));
        assert_eq!(
            reply.ended,
            vec![ScopeEnded {
                scope_ref: "s".into(),
                scope_epoch: 1
            }]
        );
        assert_eq!(status(&table, OWNER, "s"), ScopeStatus::Ended);
        assert_eq!(status(&table, OTHER, "s"), ScopeStatus::Live);
        assert!(reply.tag_changes.iter().all(|change| change.owner == OWNER));
        let reply = apply(&mut table, 3, vec![], vec![ScopeEnd::new("s", 1)]);
        assert_eq!(reply.end_results[0].outcome, ScopeEndOutcome::NotLive);
        assert!(reply.ended.is_empty());
        let reply = apply(&mut table, 4, vec![record("s", 2)], vec![]);
        assert_eq!(reply.results[0].outcome, ScopeRecordOutcome::Created);
        assert_eq!(
            code(&apply(&mut table, 5, vec![record("s", 1)], vec![])),
            Some(error_codes::SCOPE_EXPIRED)
        );
    }
}

#[test]
fn accepted_apply_end_of_unswept_expiry_is_not_live_and_higher_epoch_is_created() {
    for replace in [false, true] {
        let (mut table, clock) = clocked_table();
        sync(
            &mut table,
            OWNER,
            1,
            vec![record("s", 1).with_expires_at_ms(Some(2_000))],
        );
        clock.store(2_000, Ordering::SeqCst);
        let reply = if replace {
            apply(&mut table, 2, vec![record("s", 2)], vec![])
        } else {
            apply(&mut table, 2, vec![], vec![ScopeEnd::new("s", 1)])
        };
        assert_eq!(reply.ended.len(), 1);
        if replace {
            assert_eq!(reply.results[0].outcome, ScopeRecordOutcome::Created);
        } else {
            assert_eq!(reply.end_results[0].outcome, ScopeEndOutcome::NotLive);
        }
    }
}

#[test]
fn admission_refuses_passed_deadline_without_mutating_the_table() {
    let (mut table, clock) = clocked_table();
    sync(
        &mut table,
        OWNER,
        1,
        vec![record("s", 1).with_expires_at_ms(Some(2_000))],
    );
    let selector = ScopeSelector::new(reserved(OWNER), "s", Some(1));
    clock.store(2_000, Ordering::SeqCst);
    let refusal = table
        .admit(&reserved(OWNER), "provider", &selector, true)
        .unwrap_err();
    assert_eq!(refusal.code, error_codes::SCOPE_NOT_LIVE);
    assert!(
        refusal.message.contains("'s'")
            && refusal.message.contains("2000")
            && refusal.message.contains("passed")
    );
    assert_eq!(status(&table, OWNER, "s"), ScopeStatus::Live);
    clock.store(1_999, Ordering::SeqCst);
    assert!(table
        .admit(&reserved(OWNER), "provider", &selector, true)
        .is_ok());
}

#[test]
fn expiry_immutability_precedes_new_deadline_checks_and_horizon_is_overflow_safe() {
    let (mut table, clock) = clocked_table();
    sync(
        &mut table,
        OWNER,
        1,
        vec![
            record("s", 1).with_expires_at_ms(Some(2_000)),
            record("plain", 1),
        ],
    );
    for (generation, deadline) in [
        (2, None),
        (3, Some(3_000)),
        (4, Some(1)),
        (5, Some(u64::MAX)),
    ] {
        let reply = apply(
            &mut table,
            generation,
            vec![record("s", 1).with_expires_at_ms(deadline)],
            vec![],
        );
        assert_eq!(code(&reply), Some(error_codes::SCOPE_EXPIRY_IMMUTABLE));
        assert_eq!(
            table.owners[OWNER].live["s"].record.expires_at_ms,
            Some(2_000)
        );
    }
    let reply = apply(
        &mut table,
        6,
        vec![record("plain", 1).with_expires_at_ms(Some(2_000))],
        vec![],
    );
    assert_eq!(code(&reply), Some(error_codes::SCOPE_EXPIRY_IMMUTABLE));
    let horizon = 1_000 + MAX_SCOPE_EXPIRY_AHEAD_MS;
    let reply = apply(
        &mut table,
        7,
        vec![record("s", 2).with_expires_at_ms(Some(horizon))],
        vec![],
    );
    assert_eq!(reply.results[0].outcome, ScopeRecordOutcome::Replaced);
    clock.store(0, Ordering::SeqCst);
    let reply = apply(
        &mut table,
        8,
        vec![record("s", 2).with_expires_at_ms(Some(horizon))],
        vec![],
    );
    assert_eq!(reply.results[0].outcome, ScopeRecordOutcome::Unchanged);
    clock.store(1_000, Ordering::SeqCst);
    for (generation, deadline) in [(9, horizon + 1), (10, u64::MAX)] {
        assert_eq!(
            code(&apply(
                &mut table,
                generation,
                vec![record("new", 1).with_expires_at_ms(Some(deadline))],
                vec![]
            )),
            Some(error_codes::SCOPE_EXPIRY_TOO_FAR)
        );
    }
}

#[test]
fn fresh_store_rejects_elapsed_expiry_but_accepts_absent_and_future_deadlines() {
    let (mut table, _) = clocked_table();
    for (generation, deadline, expected) in [
        (1, Some(1_000), ScopeRecordOutcome::Refused),
        (2, None, ScopeRecordOutcome::Created),
        (3, Some(2_000), ScopeRecordOutcome::Replaced),
    ] {
        let reply = sync(
            &mut table,
            OWNER,
            generation,
            vec![record("s", generation).with_expires_at_ms(deadline)],
        );
        assert_eq!(reply.results[0].outcome, expected);
        if expected == ScopeRecordOutcome::Refused {
            assert_eq!(code(&reply), Some(error_codes::SCOPE_EXPIRED));
        }
    }
}

#[test]
fn early_end_and_omission_keep_owner_ended_tombstone_causes() {
    for delta in [false, true] {
        let (mut table, clock) = clocked_table();
        let scope = record("s", 1).with_expires_at_ms(Some(2_000));
        sync(&mut table, OWNER, 1, vec![scope.clone()]);
        if delta {
            apply(&mut table, 2, vec![], vec![ScopeEnd::new("s", 1)]);
        } else {
            sync(&mut table, OWNER, 2, vec![]);
        }
        clock.store(3_000, Ordering::SeqCst);
        assert_eq!(
            code(&apply(&mut table, 3, vec![scope], vec![])),
            Some(error_codes::SCOPE_EPOCH_ENDED)
        );
    }
}

#[test]
fn scope_apply_whole_call_refusals_preserve_authority_generation_and_unswept_scope() {
    let (mut table, clock) = clocked_table();
    assert_eq!(
        table
            .apply(OWNER, ConnectionId::new(1), |_| true, 0, vec![], vec![])
            .unwrap_err()
            .code,
        error_codes::SCOPE_SYNC_REQUIRED
    );
    assert_eq!(
        table
            .apply(OWNER, ConnectionId::new(1), |_| false, 0, vec![], vec![])
            .unwrap_err()
            .code,
        error_codes::SCOPE_SYNC_NOT_AUTHORITY
    );
    let scope = record("s", 1).with_expires_at_ms(Some(2_000));
    sync(&mut table, OWNER, 20, vec![scope.clone()]);
    clock.store(2_000, Ordering::SeqCst);
    let authority = table.owners[OWNER].authority;
    for (generation, upsert, end, expected) in [
        (20, vec![], vec![], error_codes::SCOPE_SYNC_STALE),
        (
            21,
            vec![record("x", 1), record("x", 2)],
            vec![],
            INVALID_CONTROL_BODY,
        ),
        (
            21,
            vec![],
            vec![ScopeEnd::new("x", 1), ScopeEnd::new("x", 2)],
            INVALID_CONTROL_BODY,
        ),
        (
            21,
            vec![record("x", 1)],
            vec![ScopeEnd::new("x", 1)],
            INVALID_CONTROL_BODY,
        ),
        (21, vec![], vec![ScopeEnd::new("", 1)], INVALID_CONTROL_BODY),
        (21, vec![record("", 1)], vec![], INVALID_CONTROL_BODY),
        (
            21,
            vec![record("x", 1)
                .with_attributes(ScopeAttributes::new().with_run_id(Some("bad token".into())))],
            vec![],
            INVALID_CONTROL_BODY,
        ),
        (
            21,
            vec![record("x", 1).with_attributes(
                ScopeAttributes::new().with_agent_id(Some("x".repeat(MAX_SCOPE_ATTRIBUTE_BYTES))),
            )],
            vec![],
            error_codes::SCOPE_ATTRIBUTES_TOO_LARGE,
        ),
        (
            21,
            (0..=MAX_LIVE_SCOPES_PER_OWNER)
                .map(|i| record(&i.to_string(), 1))
                .collect(),
            vec![],
            error_codes::SCOPE_LIVE_LIMIT_EXCEEDED,
        ),
        (
            21,
            vec![],
            (0..=MAX_LIVE_SCOPES_PER_OWNER)
                .map(|i| ScopeEnd::new(i.to_string(), 1))
                .collect(),
            error_codes::SCOPE_LIVE_LIMIT_EXCEEDED,
        ),
    ] {
        let refusal = table
            .apply(
                OWNER,
                ConnectionId::new(1),
                |_| true,
                generation,
                upsert,
                end,
            )
            .unwrap_err();
        assert_eq!(refusal.code, expected);
        assert_eq!(table.owners[OWNER].authority, authority);
        assert_eq!(status(&table, OWNER, "s"), ScopeStatus::Live);
    }
    table.release_connection(ConnectionId::new(1));
    assert_eq!(
        table
            .apply(OWNER, ConnectionId::new(2), |_| true, 0, vec![], vec![])
            .unwrap_err()
            .code,
        error_codes::SCOPE_SYNC_REQUIRED
    );
    assert!(table.owners[OWNER].authority.is_none());
    assert_eq!(status(&table, OWNER, "s"), ScopeStatus::Live);
    let reply = table
        .sync(OWNER, ConnectionId::new(2), |_| true, 0, vec![scope])
        .unwrap();
    assert_eq!(code(&reply), Some(error_codes::SCOPE_EXPIRED));
    assert_eq!(reply.ended.len(), 1);
}

#[test]
fn scope_apply_live_limit_rollback_includes_expiry_and_frees_slots_for_accepted_calls() {
    let (mut table, clock) = clocked_table();
    let mut records: Vec<_> = (0..MAX_LIVE_SCOPES_PER_OWNER)
        .map(|i| record(&i.to_string(), 1))
        .collect();
    records[0].expires_at_ms = Some(2_000);
    sync(&mut table, OWNER, 1, records);
    clock.store(2_000, Ordering::SeqCst);
    let version = table.last_version;
    let refusal = table
        .apply(
            OWNER,
            ConnectionId::new(1),
            |_| true,
            2,
            vec![record("new1", 1), record("new2", 1)],
            vec![],
        )
        .unwrap_err();
    assert_eq!(refusal.code, error_codes::SCOPE_LIVE_LIMIT_EXCEEDED);
    assert_eq!(status(&table, OWNER, "0"), ScopeStatus::Live);
    assert_eq!(table.owners[OWNER].authority.unwrap().last_generation, 1);
    assert_eq!(table.last_version, version);
    let reply = apply(&mut table, 2, vec![record("new1", 1)], vec![]);
    assert_eq!(
        reply.ended,
        vec![ScopeEnded {
            scope_ref: "0".into(),
            scope_epoch: 1
        }]
    );
    assert_eq!(reply.results[0].outcome, ScopeRecordOutcome::Created);
    let reply = apply(
        &mut table,
        3,
        vec![record("new2", 1)],
        vec![ScopeEnd::new("1", 1)],
    );
    assert_eq!(reply.end_results[0].outcome, ScopeEndOutcome::Ended);
    assert_eq!(table.owners[OWNER].live.len(), MAX_LIVE_SCOPES_PER_OWNER);
}

#[test]
fn scope_apply_upserts_ends_and_full_sync_share_outcomes_and_generation() {
    let (mut table, _) = clocked_table();
    sync(&mut table, OWNER, 1, vec![record("held", 1)]);
    let parent = record("parent", 1).with_child_owners(vec![reserved(OTHER)]);
    let child =
        record("child", 1).with_parent(Some(ScopeParent::new(reserved(OWNER), "parent", 1)));
    let reply = apply(
        &mut table,
        2,
        vec![child, parent],
        vec![ScopeEnd::new("missing", 1)],
    );
    assert!(reply
        .results
        .iter()
        .all(|entry| entry.outcome == ScopeRecordOutcome::Created));
    assert_eq!(reply.end_results[0].outcome, ScopeEndOutcome::NotLive);
    let reply = apply(
        &mut table,
        3,
        vec![record("held", 1).with_carriers(vec![ScopeCarrier::new(reserved(OTHER))])],
        vec![],
    );
    assert_eq!(reply.results[0].outcome, ScopeRecordOutcome::Updated);
    let reply = apply(
        &mut table,
        4,
        vec![record("held", 2)],
        vec![ScopeEnd::new("parent", 1)],
    );
    assert_eq!(reply.results[0].outcome, ScopeRecordOutcome::Replaced);
    assert_eq!(reply.end_results[0].outcome, ScopeEndOutcome::Ended);
    assert_eq!(reply.ended.len(), 2);
    assert_eq!(status(&table, OWNER, "child"), ScopeStatus::Live);
    assert_eq!(
        table
            .describe(&reserved(OWNER), "child")
            .stamp
            .unwrap()
            .parent_state,
        Some(ParentState::Ended)
    );
    let reply = apply(&mut table, 5, vec![record("held", 2)], vec![]);
    assert_eq!(reply.results[0].outcome, ScopeRecordOutcome::Unchanged);
    apply(&mut table, 6, vec![], vec![]);
    assert_eq!(
        table
            .sync(OWNER, ConnectionId::new(1), |_| true, 6, vec![])
            .unwrap_err()
            .code,
        error_codes::SCOPE_SYNC_STALE
    );
    assert_eq!(sync(&mut table, OWNER, 7, vec![]).ended.len(), 2);
}

#[test]
fn scope_apply_checks_parent_links_and_cycles_against_post_delta_set() {
    let (mut table, _) = clocked_table();
    sync(
        &mut table,
        OWNER,
        1,
        vec![record("parent", 1), record("a", 1)],
    );
    let child =
        record("child", 1).with_parent(Some(ScopeParent::new(reserved(OWNER), "parent", 1)));
    let reply = apply(&mut table, 2, vec![child], vec![ScopeEnd::new("parent", 1)]);
    assert_eq!(code(&reply), Some(error_codes::SCOPE_PARENT_NOT_PERMITTED));
    let a = record("a", 1).with_parent(Some(ScopeParent::new(reserved(OWNER), "b", 1)));
    let b = record("b", 1).with_parent(Some(ScopeParent::new(reserved(OWNER), "a", 1)));
    let reply = apply(&mut table, 3, vec![a, b], vec![]);
    assert!(reply
        .results
        .iter()
        .any(|result| result.code.as_deref() == Some(error_codes::SCOPE_PARENT_NOT_PERMITTED)));
}

#[test]
fn on_behalf_of_agent_with_agent_id_is_refused_by_sync_and_apply() {
    for delta in [false, true] {
        let (mut table, _) = clocked_table();
        sync(&mut table, OWNER, 1, vec![]);
        let invalid = record("s", 1).with_attributes(
            ScopeAttributes::new()
                .with_on_behalf_of_agent(Some("head".into()))
                .with_agent_id(Some("agent".into())),
        );
        let reply = if delta {
            apply(&mut table, 2, vec![invalid], vec![])
        } else {
            sync(&mut table, OWNER, 2, vec![invalid])
        };
        assert_eq!(
            code(&reply),
            Some(error_codes::SCOPE_ATTRIBUTE_NOT_PERMITTED)
        );
        assert_eq!(status(&table, OWNER, "s"), ScopeStatus::NotLive);
    }
}

#[test]
fn on_behalf_of_agent_forbids_delegates_flow_and_run_and_requires_authority() {
    let attribution = ScopeAttributes::new().with_on_behalf_of_agent(Some("head".into()));
    for delta in [false, true] {
        for (owner, attributes) in [
            (OWNER, attribution.clone().with_delegates(true)),
            (OWNER, attribution.clone().with_flow_id(Some("flow".into()))),
            (OWNER, attribution.clone().with_run_id(Some("run".into()))),
            (OTHER, attribution.clone()),
        ] {
            let (mut table, _) = clocked_table();
            sync(&mut table, owner, 1, vec![]);
            let invalid = record("s", 1).with_attributes(attributes);
            let reply = if delta {
                table
                    .apply(
                        owner,
                        ConnectionId::new(1),
                        |_| true,
                        2,
                        vec![invalid],
                        vec![],
                    )
                    .unwrap()
            } else {
                sync(&mut table, owner, 2, vec![invalid])
            };
            assert_eq!(
                code(&reply),
                Some(error_codes::SCOPE_ATTRIBUTE_NOT_PERMITTED)
            );
            assert_eq!(status(&table, owner, "s"), ScopeStatus::NotLive);
        }
    }
}

#[test]
fn on_behalf_of_agent_validates_token_bounds_in_sync_and_apply() {
    for delta in [false, true] {
        for token in [
            "".into(),
            "bad token".into(),
            "badé".into(),
            "x".repeat(257),
        ] {
            let (mut table, _) = clocked_table();
            sync(&mut table, OWNER, 1, vec![]);
            let invalid = record("s", 1)
                .with_attributes(ScopeAttributes::new().with_on_behalf_of_agent(Some(token)));
            let refusal = if delta {
                table.apply(
                    OWNER,
                    ConnectionId::new(1),
                    |_| true,
                    2,
                    vec![invalid],
                    vec![],
                )
            } else {
                table.sync(OWNER, ConnectionId::new(1), |_| true, 2, vec![invalid])
            }
            .unwrap_err();
            assert_eq!(refusal.code, INVALID_CONTROL_BODY);
            assert!(refusal.message.contains("on_behalf_of_agent"));
        }
        for token in ["!~Agent:7/worker".into(), "x".repeat(256)] {
            let (mut table, _) = clocked_table();
            sync(&mut table, OWNER, 1, vec![]);
            let valid = record("s", 1)
                .with_attributes(ScopeAttributes::new().with_on_behalf_of_agent(Some(token)));
            let reply = if delta {
                apply(&mut table, 2, vec![valid], vec![])
            } else {
                sync(&mut table, OWNER, 2, vec![valid])
            };
            assert_eq!(reply.results[0].outcome, ScopeRecordOutcome::Created);
        }
    }
}

#[test]
fn on_behalf_of_agent_changes_move_version_and_drain_but_resends_do_not() {
    for delta in [false, true] {
        let (mut table, _) = clocked_table();
        sync(&mut table, OWNER, 1, vec![record("s", 1)]);
        let mut previous = table.owners[OWNER].live["s"].version;
        for (generation, agent) in [(2, Some("head")), (4, Some("other-head")), (6, None)] {
            let changed = record("s", 1).with_attributes(
                ScopeAttributes::new().with_on_behalf_of_agent(agent.map(str::to_string)),
            );
            let reply = if delta {
                apply(&mut table, generation, vec![changed.clone()], vec![])
            } else {
                sync(&mut table, OWNER, generation, vec![changed.clone()])
            };
            let next = reply.results[0].version.unwrap();
            assert!(next > previous);
            assert_eq!(
                reply.tag_changes[0].drain,
                ScopeDrain::All(RouteCloseReason::ScopeDelegationChanged)
            );
            previous = next;
            let resend = if delta {
                apply(&mut table, generation + 1, vec![changed], vec![])
            } else {
                sync(&mut table, OWNER, generation + 1, vec![changed])
            };
            assert_eq!(resend.results[0].outcome, ScopeRecordOutcome::Unchanged);
            assert_eq!(resend.results[0].version, Some(previous));
            assert!(resend.tag_changes.is_empty());
        }
    }
}

#[test]
fn run_id_rules_and_expiry_attribute_authority_are_distinct() {
    let (mut table, _) = clocked_table();
    let run = ScopeAttributes::new().with_run_id(Some("run:7".into()));
    let with_agent = run.clone().with_agent_id(Some("agent".into()));
    for (generation, attributes, expected) in [
        (1, run.clone(), error_codes::SCOPE_RUN_ID_WITHOUT_AGENT),
        (
            2,
            with_agent
                .clone()
                .with_flow_id(Some("flow".into()))
                .with_delegates(true),
            error_codes::SCOPE_RUN_ID_WITH_FLOW_ID,
        ),
    ] {
        assert_eq!(
            code(&sync(
                &mut table,
                OWNER,
                generation,
                vec![record("s", 1).with_attributes(attributes)]
            )),
            Some(expected)
        );
    }
    let reply = sync(
        &mut table,
        OWNER,
        3,
        vec![record("s", 1).with_attributes(with_agent.clone().with_delegates(true))],
    );
    assert_eq!(reply.results[0].outcome, ScopeRecordOutcome::Created);
    assert!(
        table
            .describe(&reserved(OWNER), "s")
            .stamp
            .unwrap()
            .attributes
            .delegates
    );
    let reply = sync(
        &mut table,
        OTHER,
        1,
        vec![record("s", 1).with_expires_at_ms(Some(2_000))],
    );
    assert_eq!(reply.results[0].outcome, ScopeRecordOutcome::Created);
    let reply = sync(
        &mut table,
        OTHER,
        2,
        vec![record("s", 1)
            .with_expires_at_ms(Some(2_000))
            .with_attributes(run)],
    );
    assert_eq!(
        code(&reply),
        Some(error_codes::SCOPE_ATTRIBUTE_NOT_PERMITTED)
    );
    assert!(reply.results[0]
        .message
        .as_ref()
        .unwrap()
        .contains("run_id"));
    for delta in [false, true] {
        let bad = record("bad", 1)
            .with_attributes(with_agent.clone().with_run_id(Some("bad token".into())));
        let refusal = if delta {
            table.apply(OWNER, ConnectionId::new(1), |_| true, 4, vec![bad], vec![])
        } else {
            table.sync(OWNER, ConnectionId::new(1), |_| true, 4, vec![bad])
        }
        .unwrap_err();
        assert_eq!(refusal.code, INVALID_CONTROL_BODY);
        assert!(refusal.message.contains("run_id"));
    }
}

#[test]
fn run_id_changes_move_version_and_drain_delegation_but_resends_do_not() {
    let (mut table, _) = clocked_table();
    let plain =
        record("s", 1).with_attributes(ScopeAttributes::new().with_agent_id(Some("agent".into())));
    sync(&mut table, OWNER, 1, vec![plain.clone()]);
    let mut previous = table.owners[OWNER].live["s"].version;
    for (generation, run_id) in [(2, Some("run:1")), (3, Some("run:2")), (4, None)] {
        let record = plain.clone().with_attributes(
            plain
                .attributes
                .clone()
                .with_run_id(run_id.map(str::to_string)),
        );
        let reply = apply(&mut table, generation, vec![record.clone()], vec![]);
        let next = reply.results[0].version.unwrap();
        assert!(next > previous);
        assert_eq!(
            reply.tag_changes[0].drain,
            ScopeDrain::All(RouteCloseReason::ScopeDelegationChanged)
        );
        previous = next;
        let (mut resend_table, _) = clocked_table();
        sync(&mut resend_table, OWNER, 1, vec![record.clone()]);
        let reply = apply(&mut resend_table, 2, vec![record], vec![]);
        assert_eq!(reply.results[0].outcome, ScopeRecordOutcome::Unchanged);
        assert!(reply.tag_changes.is_empty());
    }
}

#[cfg(unix)]
#[test]
fn scope_workdir_canonicalizes_symlinks_and_missing_tail_for_any_owner() {
    let dir = cortexkit_test_support::ScratchDir::new("scope-workdir");
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let expected = std::fs::canonicalize(&real).unwrap().join("missing");
    let mut table = ScopeTable::new([]);
    let scope = record("s", 1)
        .with_workdir(Some(alias.join("missing").to_str().unwrap().into()))
        .with_project_id(Some("project:1".into()));
    let result = sync(&mut table, OTHER, 1, vec![scope.clone()]);
    assert_eq!(code(&result), None);
    let stamp = table.describe(&reserved(OTHER), "s").stamp.unwrap();
    assert!(!stamp.owner_authorized);
    assert_eq!(stamp.workdir.as_deref(), expected.to_str());
    assert_eq!(stamp.project_id.as_deref(), Some("project:1"));
    let same = scope.with_workdir(Some(expected.to_str().unwrap().into()));
    let result = sync(&mut table, OTHER, 2, vec![same]);
    assert_eq!(result.results[0].outcome, ScopeRecordOutcome::Unchanged);
}

#[cfg(unix)]
#[test]
fn scope_workdir_refuses_relative_and_unresolvable_paths_without_replacing_state() {
    let dir = cortexkit_test_support::ScratchDir::new("scope-workdir");
    let loop_path = dir.path().join("loop");
    std::os::unix::fs::symlink(&loop_path, &loop_path).unwrap();
    let mut table = ScopeTable::new([]);
    sync(&mut table, OWNER, 1, vec![record("s", 1)]);
    for (generation, path) in [(2, "relative"), (3, loop_path.to_str().unwrap())] {
        let result = sync(
            &mut table,
            OWNER,
            generation,
            vec![record("s", 2).with_workdir(Some(path.into()))],
        );
        assert_eq!(code(&result), Some(error_codes::INVALID_WORKDIR));
        assert_eq!(table.describe(&reserved(OWNER), "s").scope_epoch, Some(1));
    }
}

#[test]
fn scope_workdir_and_project_are_immutable_including_absence_until_a_new_epoch() {
    let dir = cortexkit_test_support::ScratchDir::new("scope-workdir");
    let folder = std::fs::canonicalize(dir.path())
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    for delta in [false, true] {
        for populated in [false, true] {
            let mut table = ScopeTable::new([]);
            let empty = record("s", 1);
            let filled = empty
                .clone()
                .with_workdir(Some(folder.clone()))
                .with_project_id(Some("project:1".into()));
            let original = if populated {
                filled.clone()
            } else {
                empty.clone()
            };
            let other = if populated { empty } else { filled };
            sync(&mut table, OWNER, 1, vec![original.clone()]);
            let mut generation = 1;
            for (workdir, project_id, expected) in [
                (
                    other.workdir.clone(),
                    original.project_id.clone(),
                    error_codes::SCOPE_WORKDIR_CHANGED,
                ),
                (
                    Some(format!("{folder}/other")),
                    original.project_id.clone(),
                    error_codes::SCOPE_WORKDIR_CHANGED,
                ),
                (
                    original.workdir.clone(),
                    other.project_id.clone(),
                    error_codes::SCOPE_PROJECT_CHANGED,
                ),
                (
                    original.workdir.clone(),
                    Some("project:2".into()),
                    error_codes::SCOPE_PROJECT_CHANGED,
                ),
            ] {
                generation += 1;
                let proposed = original
                    .clone()
                    .with_workdir(workdir)
                    .with_project_id(project_id);
                let result = if delta {
                    apply(&mut table, generation, vec![proposed], vec![])
                } else {
                    sync(&mut table, OWNER, generation, vec![proposed])
                };
                assert_eq!(code(&result), Some(expected));
                let held = table.describe(&reserved(OWNER), "s").stamp.unwrap();
                assert_eq!(held.workdir, original.workdir);
                assert_eq!(held.project_id, original.project_id);
            }
            let mut next = other;
            next.scope_epoch = 2;
            let result = if delta {
                apply(&mut table, generation + 1, vec![next.clone()], vec![])
            } else {
                sync(&mut table, OWNER, generation + 1, vec![next.clone()])
            };
            assert_eq!(code(&result), None);
            let stamp = table.describe(&reserved(OWNER), "s").stamp.unwrap();
            assert_eq!(stamp.workdir, next.workdir);
            assert_eq!(stamp.project_id, next.project_id);
        }
    }
}

#[test]
fn scope_project_id_validation_refuses_sync_and_apply() {
    let mut table = ScopeTable::new([]);
    sync(&mut table, OWNER, 1, vec![record("s", 1)]);
    for id in ["", "a b", "a\t", "é", &"p".repeat(129)] {
        let scope = record("s", 2).with_project_id(Some(id.into()));
        assert_eq!(
            table
                .sync(
                    OWNER,
                    ConnectionId::new(1),
                    |_| true,
                    2,
                    vec![scope.clone()]
                )
                .unwrap_err()
                .code,
            INVALID_CONTROL_BODY
        );
        assert_eq!(
            table
                .apply(
                    OWNER,
                    ConnectionId::new(1),
                    |_| true,
                    2,
                    vec![scope],
                    vec![]
                )
                .unwrap_err()
                .code,
            INVALID_CONTROL_BODY
        );
        assert_eq!(table.describe(&reserved(OWNER), "s").scope_epoch, Some(1));
    }
}

#[cfg(unix)]
#[test]
fn scope_workdir_table_stores_input_verbatim_without_filesystem_resolution() {
    let dir = cortexkit_test_support::ScratchDir::new("scope-table-workdir");
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let workdir = alias.join("missing").to_str().unwrap().to_owned();
    let mut table = ScopeTable::new([]);
    let scope = record("s", 1).with_workdir(Some(workdir.clone()));
    let result = table
        .sync(
            OWNER,
            ConnectionId::new(1),
            |_| true,
            1,
            vec![scope.clone()],
        )
        .unwrap();
    assert_eq!(code(&result), None);
    assert_eq!(
        table
            .describe(&reserved(OWNER), "s")
            .stamp
            .unwrap()
            .workdir
            .as_deref(),
        Some(workdir.as_str())
    );
    let result = table
        .apply(
            OWNER,
            ConnectionId::new(1),
            |_| true,
            2,
            vec![scope],
            vec![],
        )
        .unwrap();
    assert_eq!(result.results[0].outcome, ScopeRecordOutcome::Unchanged);
    let loop_path = dir.path().join("loop");
    std::os::unix::fs::symlink(&loop_path, &loop_path).unwrap();
    let unresolvable = loop_path.to_str().unwrap().to_owned();
    let result = table
        .apply(
            OWNER,
            ConnectionId::new(1),
            |_| true,
            3,
            vec![record("s", 2).with_workdir(Some(unresolvable.clone()))],
            vec![],
        )
        .unwrap();
    assert_eq!(code(&result), None);
    assert_eq!(
        table
            .describe(&reserved(OWNER), "s")
            .stamp
            .unwrap()
            .workdir
            .as_deref(),
        Some(unresolvable.as_str())
    );
}
