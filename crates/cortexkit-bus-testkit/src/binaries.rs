use crate::RigError;
use std::path::PathBuf;

fn find(what: &'static str, env_var: &'static str, search_path: bool) -> Result<PathBuf, RigError> {
    let configured = std::env::var_os(env_var).filter(|value| !value.is_empty());
    let path = configured.map(PathBuf::from).or_else(|| {
        search_path
            .then(|| {
                find_in_dirs(
                    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
                    what,
                )
            })
            .flatten()
    });
    let path = path
        .filter(|path| path.is_file())
        .ok_or_else(|| RigError::MissingBinary {
            what,
            env_var,
            how_to_fix: format!(
                "set {env_var} to an existing test-build executable{}",
                if search_path {
                    " or install nats-server >= 2.10 on PATH"
                } else {
                    " (no installed ck-* fallback)"
                }
            ),
        })?;
    Ok(path)
}

/// The first directory holding `name` as an executable file. Windows names executables
/// with `.exe`, so a PATH search for `nats-server` there must look for `nats-server.exe`.
fn find_in_dirs(dirs: impl IntoIterator<Item = PathBuf>, name: &str) -> Option<PathBuf> {
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    dirs.into_iter()
        .map(|dir| dir.join(&file))
        .find(|path| path.is_file())
}

pub(crate) fn ck_bus_source() -> Result<PathBuf, RigError> {
    find("ck-bus", "CK_BUS_BIN", false)
}

pub fn ck_bus_binary() -> Result<PathBuf, RigError> {
    ck_bus_source().map(cortexkit_test_support::ckdev_binary)
}
pub fn ck_subc_binary() -> Result<PathBuf, RigError> {
    find("ck-subc", "CK_SUBC_BIN", false).map(cortexkit_test_support::ckdev_binary)
}

/// Returns a staged executable and its reported version. An old or unrecognizable
/// server is a configuration error, not a missing prerequisite.
pub fn nats_server_bin() -> Result<(PathBuf, String), RigError> {
    let path = find("nats-server", "CK_NATS_SERVER_BIN", true)?;
    checked_nats(path)
}

pub(crate) fn checked_nats(path: PathBuf) -> Result<(PathBuf, String), RigError> {
    if !path.is_file() {
        return Err(RigError::MissingBinary {
            what: "nats-server", env_var: "CK_NATS_SERVER_BIN",
            how_to_fix: format!("set CK_NATS_SERVER_BIN to an existing nats-server >= 2.10 test executable; {} does not exist", path.display()),
        });
    }
    let path = cortexkit_test_support::ckdev_binary(path);
    let output = std::process::Command::new(&path)
        .arg("--version")
        .output()?;
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let numbers: Vec<u64> = version
        .rsplit('v')
        .next()
        .unwrap_or_default()
        .split('.')
        .filter_map(|part| part.parse().ok())
        .collect();
    match numbers.as_slice() {
        [major, minor, ..] if output.status.success() && (*major, *minor) >= (2, 10) => {
            Ok((path, version))
        }
        _ => Err(RigError::Invalid(format!(
            "nats-server >= 2.10 required; observed {version:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::find_in_dirs;

    #[test]
    fn path_search_finds_the_platform_executable_name() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        // A file without the platform's executable suffix must not count on Windows.
        if !std::env::consts::EXE_SUFFIX.is_empty() {
            std::fs::write(first.path().join("nats-server"), b"").unwrap();
        }
        let expected = second
            .path()
            .join(format!("nats-server{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&expected, b"").unwrap();
        let dirs = [first.path().to_path_buf(), second.path().to_path_buf()];
        assert_eq!(find_in_dirs(dirs.clone(), "nats-server"), Some(expected));
        assert_eq!(find_in_dirs(dirs, "ck-bus"), None);
    }
}
