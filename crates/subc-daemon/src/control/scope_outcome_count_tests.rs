use super::*;

fn result(outcome: ScopeRecordOutcome) -> ScopeRecordResult {
    ScopeRecordResult {
        scope_ref: "r".to_string(),
        scope_epoch: 1,
        outcome,
        code: None,
        message: None,
        version: None,
        parent_state: None,
    }
}

/// Each outcome lands in its own count, so a refused record can never be
/// hidden inside the total the log already printed.
#[test]
fn every_outcome_is_counted_in_its_own_field() {
    let results = [
        result(ScopeRecordOutcome::Created),
        result(ScopeRecordOutcome::Created),
        result(ScopeRecordOutcome::Replaced),
        result(ScopeRecordOutcome::Updated),
        result(ScopeRecordOutcome::Unchanged),
        result(ScopeRecordOutcome::Refused),
        result(ScopeRecordOutcome::Refused),
        result(ScopeRecordOutcome::Refused),
    ];
    assert_eq!(
        ScopeOutcomeCounts::of(&results),
        ScopeOutcomeCounts {
            created: 2,
            replaced: 1,
            updated: 1,
            unchanged: 1,
            refused: 3,
        }
    );
}
