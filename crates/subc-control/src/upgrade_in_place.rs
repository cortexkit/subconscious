//! Error codes for the `daemon.upgrade_in_place` operator request.

pub const UPGRADE_IN_PLACE_UNSUPPORTED: &str = "upgrade_in_place_unsupported";
pub const UPGRADE_EXECUTABLE_MISMATCH: &str = "upgrade_executable_mismatch";
pub const UPGRADE_SIGNATURE_MISMATCH: &str = "upgrade_signature_mismatch";
pub const UPGRADE_TARGET_INCOMPATIBLE: &str = "upgrade_target_incompatible";
/// The daemon refused the attempt before changing anything. `detail.reason` names the blocker.
pub const UPGRADE_BUSY: &str = "upgrade_busy";
/// The attempt failed. Detail reports the `stage`, `attempt_id`, `announced` and `restarted`.
pub const UPGRADE_FAILED: &str = "upgrade_failed";
