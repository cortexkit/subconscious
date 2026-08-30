use super::{
    mc_detection::{McClassification, McDetection, McPlatform},
    model::DetectionOutcome,
};

const WINDOWS_MC_OWNER_GATE: &str = "automatic MC conversion on Windows is disabled until the MC owner confirms standalone installs use the owner-pinned path";

/// Converts the read-only MC classification into the planner's mutation-neutral
/// offer state. SQLite busy means a live installation, but it never grants an
/// automatic conversion offer.
pub fn mc_detection_outcome(detection: &McDetection) -> DetectionOutcome {
    match detection.classification {
        McClassification::Tier2 if detection.evidence.platform == McPlatform::Windows => {
            DetectionOutcome::OwnerGated {
                reason: WINDOWS_MC_OWNER_GATE.to_string(),
            }
        }
        McClassification::Tier2 => DetectionOutcome::OfferConversion,
        McClassification::InstalledAndLive => DetectionOutcome::InstalledAndLive,
        McClassification::TornState | McClassification::Unknown => DetectionOutcome::Unknown,
        McClassification::Absent
        | McClassification::ForeignSqlite
        | McClassification::Malformed
        | McClassification::Tier1Empty => DetectionOutcome::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::mc_detection::{McDetectionEvidence, McEnvironment};

    fn detection(classification: McClassification, platform: McPlatform) -> McDetection {
        let mut result = super::super::mc_detection::detect(&McEnvironment::default(), platform);
        result.classification = classification;
        result.evidence = McDetectionEvidence {
            platform,
            data_directory_source: None,
            data_directory: None,
            database_path: None,
            database_present: false,
            wal_present: None,
            shm_present: None,
            read_only_uri: None,
            has_pre_fork_migration: None,
            durable_row_counts: Default::default(),
            sqlite_error: None,
        };
        result
    }

    #[test]
    fn only_non_windows_tier_two_state_offers_automatic_conversion() {
        assert_eq!(
            mc_detection_outcome(&detection(McClassification::Tier2, McPlatform::Unix)),
            DetectionOutcome::OfferConversion
        );
        assert!(matches!(
            mc_detection_outcome(&detection(McClassification::Tier2, McPlatform::Windows)),
            DetectionOutcome::OwnerGated { .. }
        ));
    }

    #[test]
    fn live_and_refusal_shaped_states_never_offer_conversion() {
        for classification in [
            McClassification::InstalledAndLive,
            McClassification::Tier1Empty,
            McClassification::ForeignSqlite,
            McClassification::Malformed,
            McClassification::TornState,
            McClassification::Absent,
            McClassification::Unknown,
        ] {
            assert!(!matches!(
                mc_detection_outcome(&detection(classification, McPlatform::Unix)),
                DetectionOutcome::OfferConversion
            ));
        }
    }
}
