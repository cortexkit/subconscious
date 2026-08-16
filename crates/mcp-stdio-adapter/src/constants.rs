//! Frozen lifecycle defaults for the stdio MCP adapter.

pub const SPAWN_INITIALIZE_BUDGET_MS: u64 = 30_000;
pub const SPAWN_ATTEMPT_BUDGET: u64 = 3;
pub const SPAWN_RETRY_COOLDOWN_MS: u64 = 60_000;
pub const CHILD_EARLY_EXIT_MS: u64 = 10_000;
pub const EVICTION_GRACE_MS: u64 = 5_000;
pub const DEFAULT_DEADLINE_MS: u64 = 120_000;
pub const DEFAULT_FRAME_CEILING_BYTES: u64 = 4 * 1024 * 1024;
pub const DEFAULT_IDLE_TTL_MS: u64 = 300_000;
pub const DEFAULT_MAX_CHILDREN: u64 = 8;

/// Unix child-environment keys that may pass through from the adapter by name.
pub const UNIX_BASE_ENV_KEYS: &[&str] = &["PATH", "HOME", "TMPDIR", "LANG"];

/// Windows child-environment keys that may pass through from the adapter by name.
pub const WINDOWS_BASE_ENV_KEYS: &[&str] = &[
    "Path",
    "SystemRoot",
    "SystemDrive",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PATHEXT",
    "ComSpec",
];

#[cfg(unix)]
pub const BASE_ENV_KEYS: &[&str] = UNIX_BASE_ENV_KEYS;
#[cfg(windows)]
pub const BASE_ENV_KEYS: &[&str] = WINDOWS_BASE_ENV_KEYS;
#[cfg(not(any(unix, windows)))]
pub const BASE_ENV_KEYS: &[&str] = &[];

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn lifecycle_defaults_match_the_settled_contract() {
        assert_eq!(SPAWN_INITIALIZE_BUDGET_MS, 30_000);
        assert_eq!(SPAWN_ATTEMPT_BUDGET, 3);
        assert_eq!(SPAWN_RETRY_COOLDOWN_MS, 60_000);
        assert_eq!(CHILD_EARLY_EXIT_MS, 10_000);
        assert_eq!(EVICTION_GRACE_MS, 5_000);
        assert_eq!(DEFAULT_DEADLINE_MS, 120_000);
        assert_eq!(DEFAULT_FRAME_CEILING_BYTES, 4 * 1024 * 1024);
        assert_eq!(DEFAULT_IDLE_TTL_MS, 300_000);
        assert_eq!(DEFAULT_MAX_CHILDREN, 8);
    }

    // The child-environment constructor extends this with its constructed map.
    #[cfg(unix)]
    #[test]
    fn unix_base_env_key_allowlist_is_frozen() {
        let expected = BTreeSet::from([
            "PATH".to_string(),
            "HOME".to_string(),
            "TMPDIR".to_string(),
            "LANG".to_string(),
        ]);
        let actual: BTreeSet<_> = BASE_ENV_KEYS.iter().map(ToString::to_string).collect();

        assert_eq!(actual, expected);
    }

    // The child-environment constructor extends this with its constructed map.
    #[cfg(windows)]
    #[test]
    fn windows_base_env_key_allowlist_is_frozen() {
        let expected = BTreeSet::from([
            "Path".to_string(),
            "SystemRoot".to_string(),
            "SystemDrive".to_string(),
            "TEMP".to_string(),
            "TMP".to_string(),
            "USERPROFILE".to_string(),
            "APPDATA".to_string(),
            "LOCALAPPDATA".to_string(),
            "PATHEXT".to_string(),
            "ComSpec".to_string(),
        ]);
        let actual: BTreeSet<_> = BASE_ENV_KEYS.iter().map(ToString::to_string).collect();

        assert_eq!(actual, expected);
    }
}
