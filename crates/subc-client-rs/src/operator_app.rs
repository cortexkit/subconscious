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

#[cfg(not(target_os = "linux"))]
fn path_error(reason: impl Into<String>) -> OperatorAppPathError {
    OperatorAppPathError {
        os: std::env::consts::OS,
        reason: reason.into(),
    }
}

/// Resolve the installed daemon's connection-file path using OS facts only.
///
/// Unlike [`super::discover`], this neither reads environment variables nor
/// searches fallback locations. It does not read the connection file itself.
/// Resolves the standard launchd (macOS) or systemd user-service (Linux) location;
/// custom daemon environment overrides are deliberately not discovered. Windows
/// returns a typed error because the installed writer's filename depends on
/// environment-derived user names, not a known-folder path.
pub fn operator_app_path() -> Result<PathBuf, OperatorAppPathError> {
    platform_path()
}

#[cfg(target_os = "macos")]
fn platform_path() -> Result<PathBuf, OperatorAppPathError> {
    // launchd supplies the per-user Darwin temporary directory to the installed
    // daemon, whose bootstrap uses temp_dir()/subc-<uid>.connection.json. confstr
    // obtains that same OS directory without consulting the app's TMPDIR.
    let directory =
        subc_os::darwin_user_temp_dir().map_err(|error| path_error(error.to_string()))?;
    Ok(directory.join(format!(
        "subc-{}.connection.json",
        subc_transport::connection_file::user_connection_token()
    )))
}

#[cfg(target_os = "linux")]
fn platform_path() -> Result<PathBuf, OperatorAppPathError> {
    // The installed cortexkit-subc.service is a systemd user unit. systemd sets
    // its XDG_RUNTIME_DIR to /run/user/<uid>; bootstrap appends this filename.
    // Read the uid from the kernel instead of trusting the app's environment.
    Ok(PathBuf::from(format!(
        "/run/user/{}",
        subc_transport::connection_file::user_connection_token()
    ))
    .join(subc_transport::connection_file::CONNECTION_FILE_NAME))
}

#[cfg(target_os = "windows")]
fn platform_path() -> Result<PathBuf, OperatorAppPathError> {
    // The installed Task Scheduler action does not pin a connection-file path.
    // bootstrap uses XDG_RUNTIME_DIR or temp_dir()/subc-<user-token>.connection.json,
    // and the token comes from USER/USERNAME/HOME/USERPROFILE. A known-folder API
    // cannot recover those environment choices, so do not guess LOCALAPPDATA.
    Err(path_error("the installed daemon chooses its directory and user-token filename from environment variables; no known-folder location is fixed"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn platform_path() -> Result<PathBuf, OperatorAppPathError> {
    Err(path_error(
        "no environment-free installed daemon location is defined for this OS",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

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
            let result = operator_app_path();
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            assert!(
                result.is_ok(),
                "OS path resolution must succeed: {result:?}"
            );
            #[cfg(target_os = "linux")]
            assert_eq!(
                result.as_ref().unwrap(),
                &PathBuf::from(format!(
                    "/run/user/{}/subc-connection.json",
                    subc_transport::connection_file::user_connection_token()
                ))
            );
            #[cfg(target_os = "macos")]
            assert_eq!(
                result.as_ref().unwrap().file_name().unwrap(),
                format!(
                    "subc-{}.connection.json",
                    subc_transport::connection_file::user_connection_token()
                )
                .as_str()
            );
            #[cfg(target_os = "windows")]
            {
                let error = result.as_ref().unwrap_err();
                assert_eq!(error.os, "windows");
                assert!(error.reason.contains("environment"));
            }
            if let Ok(path) = &result {
                assert!(path.is_absolute(), "{path:?}");
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
