use super::*;
use crate::setup::{
    model::{PlanOutcome, UpgradeOperation},
    planner::plan_upgrade,
    update_cache::CachedRelease,
};

#[test]
fn plexus_catalog_only_change_upgrades_both_programs_before_catalog_and_restart() {
    let roster = upgrade_roster([Component::Plexus]);
    let [module, admin, catalog] = roster.as_slice() else {
        panic!("{roster:?}")
    };
    assert_eq!(module.module_id(), Some("plexus"));
    assert_eq!(admin.label(), "ck-plexus-admin");
    assert_eq!(admin.module_id(), None);
    assert_eq!(catalog.label(), "ck-plexus-catalog");
    let discovered = roster
        .iter()
        .map(|&target| ManagedUpgradeTarget {
            target,
            destination: PathBuf::from(target.label()),
            installed_version: "1.0.0".into(),
            installed_archive_sha256: Some("12".repeat(32)),
            registration_channel_missing: false,
        })
        .collect::<Vec<_>>();
    let mut metadata = UpdateMetadata {
        format_version: 1,
        checked_at_unix_secs: 1,
        targets: roster
            .iter()
            .map(|&target| {
                (
                    target.label().into(),
                    CachedRelease {
                        version: "1.0.0".into(),
                        sha256: Some(
                            if target.is_plexus_catalog() {
                                "34"
                            } else {
                                "12"
                            }
                            .repeat(32),
                        ),
                        reports_release_version: !target.is_plexus_catalog(),
                    },
                )
            })
            .collect(),
    };
    let observe = |metadata: &UpdateMetadata| {
        observed_upgrade_targets(
            metadata,
            &discovered,
            Ok(BTreeSet::from(["plexus".into()])),
            None,
        )
    };
    let observed = observe(&metadata);
    let operations = plan_upgrade(&observed).operations;
    let at = |op: UpgradeOperation| {
        operations
            .iter()
            .position(|candidate| *candidate == op)
            .unwrap()
    };
    let placement = at(UpgradeOperation::RefreshPlexusCatalog { target: *catalog });
    assert!(at(UpgradeOperation::ReplaceDestination { target: *module }) < placement);
    assert!(at(UpgradeOperation::PostVerify { target: *admin }) < placement);
    assert!(placement < at(UpgradeOperation::InitiateModuleRestart { target: *module }));
    assert!(!operations.contains(&UpgradeOperation::WarmExecute { target: *catalog }));

    metadata
        .targets
        .get_mut("ck-plexus-catalog")
        .unwrap()
        .sha256 = None;
    let plan = plan_upgrade(&observe(&metadata));
    assert!(plan.outcomes.iter().any(|outcome| matches!(outcome, PlanOutcome::ReleaseIncomplete { component: Component::Plexus, missing_asset, .. } if missing_asset.starts_with("ck-plexus-catalog-"))));
    assert_eq!(plan.operations, [UpgradeOperation::ObservePlatform]);

    metadata
        .targets
        .get_mut("ck-plexus-catalog")
        .unwrap()
        .sha256 = Some("12".repeat(32));
    assert_eq!(
        plan_upgrade(&observe(&metadata)).operations,
        [UpgradeOperation::ObservePlatform]
    );
}
