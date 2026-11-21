use super::{HealthProbeError, HealthProbeEvidence};
use std::collections::HashSet;

/// The evidential asymmetry, asserted rather than described.
///
/// Exactly ONE observation is proof a module cannot serve, and the one that
/// fires under CPU starvation is not it. Before the split, all fifteen
/// construction sites collapsed into a single String, so a timeout carried the
/// same weight as a dead lane -- which is how a healthy module was restarted
/// three times in one day.
#[test]
fn only_a_dead_lane_is_proof_of_death() {
    assert!(HealthProbeError::lane_dead("gone").is_proof_of_death());
    // Three non-proof classes, each for a different reason: silence is
    // consistent with health, a bad answer proves the module ALIVE, and a
    // daemon-side fault never reached the module at all.
    assert!(!HealthProbeError::no_answer("timed out").is_proof_of_death());
    assert!(!HealthProbeError::bad_answer("garbage").is_proof_of_death());
    assert!(!HealthProbeError::misconfigured("no table").is_proof_of_death());
}

/// Labels must be distinct, or the operator-facing distinction is cosmetic.
///
/// A shared label renders two different observations identically in the line an
/// operator reads after an unexplained restart -- the exact confusion this
/// change removes.
#[test]
fn every_evidence_class_has_a_distinct_label() {
    let labels = [
        HealthProbeError::lane_dead("").label(),
        HealthProbeError::no_answer("").label(),
        HealthProbeError::bad_answer("").label(),
        HealthProbeError::misconfigured("").label(),
    ];
    let unique: HashSet<_> = labels.iter().collect();
    assert_eq!(unique.len(), labels.len(), "labels collided: {labels:?}");
}

/// The class is additional information, not a replacement.
///
/// An operator needs both "this was silence" and the specific text saying how
/// long we waited; a classification that swallowed the message would trade one
/// missing distinction for another.
#[test]
fn classification_preserves_the_original_message() {
    let err = HealthProbeError::no_answer("module did not answer within 5s");
    assert_eq!(err.to_string(), "module did not answer within 5s");
    assert!(matches!(err.evidence, HealthProbeEvidence::NoAnswer));
}
