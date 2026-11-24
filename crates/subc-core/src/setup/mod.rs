mod apply;
mod bus_monitoring;
mod ckni;
mod components;
mod config;
mod conversion;
mod daemon_bundle;
mod definition;
mod detection;
pub(crate) mod file_modes;
pub(crate) mod install_journal;
pub(crate) mod install_lock;
mod messages;
pub use install_lock::InstallHold;
mod inventory;
mod mc_detection;
mod message_plane;
mod model;
mod nats_migration;
mod planner;
mod prompt;
pub(crate) mod release_index;
mod retained_binary;
pub(crate) mod runtime;
pub(crate) mod systemd_unit;
#[cfg(all(test, unix))]
mod test_exec;
pub mod text;
mod uninstall;
mod update_cache;
mod update_check;
mod upgrade;
pub(crate) mod upgrade_assets;
mod upgrade_executor;
mod upgrade_verification;
mod validation;
pub mod wizard;

pub use apply::data_directory;
pub(crate) use apply::user_home;
pub use apply::{
    default_claustrum_key_path, installed_service_facts, start_installed_service, SetupBackend,
};
pub use components::{component_binaries_for_target, module_program, supervised_module_ids};
#[doc(hidden)]
pub use install_journal::bootstrap_entry;
pub use model::{
    version_transition, AlphaTarget, Component, ComponentState, PlanOutcome, PlatformObservation,
    SetupObserved, SetupRequest, UpgradeOperation, UpgradeState, UpgradeTarget,
};
pub use planner::{plan_setup, plan_upgrade, SetupPlan, UpgradePlan};
pub use update_cache::{cache_directory, UpdateCache};
pub use update_check::{
    check_update_metadata, dashboard_update, not_checked_from_cache, IndexReleaseSource,
    UpdateCheckError, BARE_REFRESH_BUDGET, TARGET_CHECK_BUDGET,
};
pub use upgrade::{
    dashboard_installed_binaries, discover_current_upgrade_targets,
    lock_and_discover_upgrade_targets, observed_upgrade_targets, render_execution_report,
    upgrade_ownership_outcomes, DaemonCatalogBuild, ManagedUpgradeTarget, SystemUpgradeBackend,
    UpgradeOutput,
};
// Production announces completion from inside the executor after verification;
// only the fixture CLI's simulated apply renders the completion line directly.
#[cfg(feature = "test-support")]
pub use upgrade::upgraded_line;
pub use upgrade_executor::{
    execute_upgrade, RollbackDecision, UpgradeEvidence, UpgradeExecutionBackend,
    UpgradeExecutionFailure, UpgradeExecutionReport,
};
pub mod operator_prompt;
