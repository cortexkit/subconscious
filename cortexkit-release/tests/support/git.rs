use std::{path::Path, process::Command};

#[cfg(windows)]
const NULL_CONFIG_PATH: &str = "NUL";
#[cfg(not(windows))]
const NULL_CONFIG_PATH: &str = "/dev/null";

/// Builds a Git command whose configuration comes only from the test repository.
///
/// Git otherwise reads both system and global configuration, and its global
/// excludes file can also be discovered below `XDG_CONFIG_HOME`. A fresh
/// per-test config home keeps those ambient inputs out while preserving the
/// repository's local configuration.
pub(crate) fn git_command(config_home: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .env("GIT_CONFIG_GLOBAL", NULL_CONFIG_PATH)
        .env("GIT_CONFIG_SYSTEM", NULL_CONFIG_PATH)
        .env("XDG_CONFIG_HOME", config_home);
    command
}
