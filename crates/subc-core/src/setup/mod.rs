mod apply;
mod components;
mod config;
mod conversion;
mod detection;
mod inventory;
mod mc_detection;
mod model;
mod planner;
mod runtime;
mod uninstall;
mod update_cache;
mod update_check;
mod validation;

pub use apply::SetupBackend;
pub use model::{Component, SetupRequest, UpgradeObserved};
pub use planner::{plan_setup, plan_upgrade, SetupPlan, UpgradePlan};
pub use update_cache::UpdateCache;
pub use update_check::{
    check_update_metadata, compiled_installed_versions, dashboard_update, not_checked_from_cache,
    observed_from_metadata, GitHubReleaseSource, UpdateCheckError,
};
