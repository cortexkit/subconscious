use std::{error::Error, fmt, path::PathBuf};

/// The installed daemon's path could not be resolved without environment input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorAppPathError {
    pub os: &'static str,
    pub reason: String,
}

impl fmt::Display for OperatorAppPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot resolve installed daemon path on {}: {}",
            self.os, self.reason
        )
    }
}

impl Error for OperatorAppPathError {}

fn path_error(reason: impl Into<String>) -> OperatorAppPathError {
    OperatorAppPathError {
        os: std::env::consts::OS,
        reason: reason.into(),
    }
}

/// Resolve the installed daemon's connection-file candidates using OS facts only.
///
/// The list follows [`subc_transport::connection_file::discovery_candidates`]
/// without the environment rungs. On macOS it tries the password-database home
/// directory's `.local/share/cortexkit/run/subc-connection.json`, then Darwin's
/// per-user temporary directory's `subc-<uid>.connection.json`. On Linux it tries
/// `/run/user/<uid>/subc-connection.json`, then the same password-home rung.
/// Neither `HOME` nor any other environment variable is read. Custom daemon
/// environment overrides are deliberately not discovered. Windows returns a
/// typed error because the installed writer's filename depends on environment-
/// derived user names, not a known-folder path. No connection files are read.
pub fn operator_app_candidates() -> Result<Vec<PathBuf>, OperatorAppPathError> {
    platform_candidates()
}

/// Resolve the first existing installed-daemon connection-file candidate, or
/// the first candidate if none exists, without reading environment variables.
///
/// Uses [`operator_app_candidates`] in order. This only checks existence, not
/// ownership or file contents; [`crate::SubcConsumer::connect_operator_app`]
/// checks ownership on every connection attempt before reading the file.
pub fn operator_app_path() -> Result<PathBuf, OperatorAppPathError> {
    let candidates = operator_app_candidates()?;
    Ok(first_existing_candidate(&candidates).to_path_buf())
}

fn first_existing_candidate(candidates: &[PathBuf]) -> &std::path::Path {
    candidates
        .iter()
        .find(|path| path.exists())
        .unwrap_or(&candidates[0])
}

pub(crate) fn owned_candidate(candidates: &[PathBuf]) -> &std::path::Path {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let uid = subc_os::effective_uid();
        owned_candidate_with(candidates, |_, meta| meta.uid() == uid)
    }
    #[cfg(not(unix))]
    {
        &candidates[0]
    }
}

#[cfg(unix)]
fn owned_candidate_with(
    candidates: &[PathBuf],
    is_owner: impl Fn(&std::path::Path, &std::fs::Metadata) -> bool,
) -> &std::path::Path {
    use std::os::unix::fs::PermissionsExt;
    // This preselection only skips missing or untrusted candidates. Transport
    // rechecks the opened inode before reading, so a path replacement cannot
    // bypass its owner check. Once selected, parse or dial errors must not fall
    // through to a different daemon.
    candidates
        .iter()
        .find(|path| {
            std::fs::metadata(path)
                .is_ok_and(|meta| is_owner(path, &meta) && meta.permissions().mode() & 0o077 == 0)
        })
        .unwrap_or(&candidates[0])
}

#[cfg(target_os = "macos")]
fn platform_candidates() -> Result<Vec<PathBuf>, OperatorAppPathError> {
    let home = subc_os::real_user_home_dir().map_err(|error| path_error(error.to_string()))?;
    let temp = subc_os::darwin_user_temp_dir().map_err(|error| path_error(error.to_string()))?;
    Ok(macos_candidates(
        &home,
        &temp,
        &subc_transport::connection_file::user_connection_token(),
    ))
}

#[cfg(target_os = "linux")]
fn platform_candidates() -> Result<Vec<PathBuf>, OperatorAppPathError> {
    let home = subc_os::real_user_home_dir().map_err(|error| path_error(error.to_string()))?;
    Ok(linux_candidates(
        &home,
        &subc_transport::connection_file::user_connection_token(),
    ))
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn macos_candidates(
    home: &std::path::Path,
    temp: &std::path::Path,
    uid: &str,
) -> Vec<PathBuf> {
    vec![
        home.join(".local/share/cortexkit/run/subc-connection.json"),
        temp.join(format!("subc-{uid}.connection.json")),
    ]
}

#[cfg(any(target_os = "linux", test))]
fn linux_candidates(home: &std::path::Path, uid: &str) -> Vec<PathBuf> {
    vec![
        PathBuf::from(format!("/run/user/{uid}/subc-connection.json")),
        home.join(".local/share/cortexkit/run/subc-connection.json"),
    ]
}

#[cfg(target_os = "windows")]
fn platform_candidates() -> Result<Vec<PathBuf>, OperatorAppPathError> {
    // The installed Task Scheduler action does not pin a connection-file path.
    // bootstrap uses XDG_RUNTIME_DIR or temp_dir()/subc-<user-token>.connection.json,
    // and the token comes from USER/USERNAME/HOME/USERPROFILE. A known-folder API
    // cannot recover those environment choices, so do not guess LOCALAPPDATA.
    Err(path_error("the installed daemon chooses its directory and user-token filename from environment variables; no known-folder location is fixed"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn platform_candidates() -> Result<Vec<PathBuf>, OperatorAppPathError> {
    Err(path_error(
        "no environment-free installed daemon location is defined for this OS",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn macos_candidate_order_from_os_facts() {
        assert_eq!(
            macos_candidates(
                std::path::Path::new("/Users/operator"),
                std::path::Path::new("/darwin/temp"),
                "501"
            ),
            vec![
                PathBuf::from("/Users/operator/.local/share/cortexkit/run/subc-connection.json"),
                PathBuf::from("/darwin/temp/subc-501.connection.json"),
            ]
        );
    }

    #[test]
    fn linux_candidate_order_from_os_facts() {
        assert_eq!(
            linux_candidates(std::path::Path::new("/home/operator"), "1001"),
            vec![
                PathBuf::from("/run/user/1001/subc-connection.json"),
                PathBuf::from("/home/operator/.local/share/cortexkit/run/subc-connection.json"),
            ]
        );
    }

    #[test]
    fn path_uses_first_existing_candidate_or_first_when_none_exist() {
        let dir = cortexkit_test_support::ScratchDir::new("operator-path");
        let candidates = macos_candidates(&dir.join("home"), &dir.join("temp"), "501");
        assert_eq!(first_existing_candidate(&candidates), candidates[0]);
        std::fs::create_dir_all(candidates[1].parent().unwrap()).unwrap();
        std::fs::write(&candidates[1], b"connection file").unwrap();
        assert_eq!(first_existing_candidate(&candidates), candidates[1]);
        std::fs::create_dir_all(candidates[0].parent().unwrap()).unwrap();
        std::fs::write(&candidates[0], b"connection file").unwrap();
        assert_eq!(first_existing_candidate(&candidates), candidates[0]);
    }

    #[cfg(unix)]
    #[test]
    fn foreign_owned_home_rung_is_skipped() {
        use cortexkit_test_support::ScratchDir;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = ScratchDir::new("operator-owner");
        let candidates = macos_candidates(&dir.join("home"), &dir.join("temp"), "501");
        for path in &candidates {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"connection file").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let expected_uid = std::fs::metadata(&candidates[1]).unwrap().uid();
        let selected = owned_candidate_with(&candidates, |path, meta| {
            let owner = if path == candidates[0] {
                expected_uid.wrapping_add(1)
            } else {
                meta.uid()
            };
            owner == expected_uid
        });
        assert_eq!(selected, candidates[1]);
        assert_eq!(owned_candidate(&candidates), candidates[0]);
    }

    #[test]
    fn operator_app_path_ignores_environment() {
        const PROBE: &str = "SUBC_OPERATOR_PATH_PROBE";
        const MARKER: &str = "operator-app-path=";
        const VARIABLES: &[&str] = &[
            "SUBC_CONNECTION_FILE",
            "XDG_RUNTIME_DIR",
            "HOME",
            "TMPDIR",
            "LOCALAPPDATA",
            "USERPROFILE",
            "USER",
            "USERNAME",
            "TMP",
            "TEMP",
        ];
        if std::env::var_os(PROBE).is_some() {
            let result = operator_app_candidates();
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            assert!(
                result.is_ok(),
                "OS path resolution must succeed: {result:?}"
            );
            #[cfg(target_os = "windows")]
            {
                let error = result.as_ref().unwrap_err();
                assert_eq!(error.os, "windows");
                assert!(error.reason.contains("environment"));
            }
            if let Ok(paths) = &result {
                assert!(paths.iter().all(|path| path.is_absolute()), "{paths:?}");
                let path = operator_app_path().unwrap();
                assert_eq!(
                    &path,
                    paths.iter().find(|path| path.exists()).unwrap_or(&paths[0])
                );
            }
            println!("{MARKER}{result:?}");
            return;
        }

        let run = |hostile: bool| {
            let mut child = Command::new(std::env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "operator_app::tests::operator_app_path_ignores_environment",
                    "--nocapture",
                ])
                .env(PROBE, "1");
            for variable in VARIABLES {
                if hostile {
                    child.env(variable, "/untrusted/operator-app-must-ignore");
                } else {
                    child.env_remove(variable);
                }
            }
            let output = child.output().unwrap();
            assert!(output.status.success(), "{output:?}");
            let stdout = String::from_utf8(output.stdout).unwrap();
            stdout
                .lines()
                .find_map(|line| line.strip_prefix(MARKER))
                .expect("path probe must run in child")
                .to_owned()
        };
        assert_eq!(run(false), run(true));
    }
}
