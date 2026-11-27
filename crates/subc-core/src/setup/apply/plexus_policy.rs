use crate::cli_output::println;
use serde_json::Value;
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{process::Command, time};

const LEASE_WAIT: Duration = Duration::from_secs(60);
const PROBE_WAIT: Duration = Duration::from_secs(5);

pub(super) fn offer(admin: &Path, connection: &Path) -> bool {
    let outcome = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())?
                    .block_on(enable_when_ready(admin, connection))
            })
            .join()
            .unwrap_or_else(|_| Err("policy check stopped unexpectedly".into()))
    });
    match &outcome {
        Ok(true) => println!("  GitHub agent policies: enabled"),
        Ok(false) => println!("  GitHub agent policies: already enabled (no change)"),
        Err(failure) => {
            match failure {
                PolicyFailure::Refused => println!("  GitHub agent policies stay off."),
                PolicyFailure::Unknown(reason) => {
                    println!("  GitHub agent policies were not confirmed enabled ({reason}).")
                }
            }
            println!(
                "  To turn them on later: {} agent-policy enable --subc {}",
                shell_path(admin),
                shell_path(connection)
            );
        }
    }
    matches!(outcome, Ok(true))
}

#[derive(Debug)]
enum PolicyFailure {
    Refused,
    Unknown(String),
}

impl From<String> for PolicyFailure {
    fn from(reason: String) -> Self {
        Self::Unknown(reason)
    }
}

impl From<&str> for PolicyFailure {
    fn from(reason: &str) -> Self {
        Self::Unknown(reason.into())
    }
}

async fn enable_when_ready(admin: &Path, connection: &Path) -> Result<bool, PolicyFailure> {
    // `connector-home status` reports `admitted`: true exactly when Plexus would
    // act now, the same check `agent-policy enable` makes before refusing. A
    // reply that fails or doesn't parse means Plexus isn't ready yet.
    let admitted = time::timeout(LEASE_WAIT, async {
        loop {
            let probe = Command::new(admin)
                .args(["connector-home", "status", "--subc"])
                .arg(connection)
                .kill_on_drop(true)
                .output();
            if matches!(time::timeout(PROBE_WAIT, probe).await, Ok(Ok(output)) if output.status.success() && ready(&output.stdout))
            {
                break;
            }
            time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await;
    if admitted.is_err() {
        return Err(
            "Plexus did not become this computer's connector host within 60 seconds".into(),
        );
    }

    let command = |verb| {
        let mut command = Command::new(admin);
        command
            .args(["agent-policy", verb, "--subc"])
            .arg(connection)
            .kill_on_drop(true);
        command
    };
    let preview = time::timeout(PROBE_WAIT, command("preview").output())
        .await
        .map_err(|_| "policy preview timed out".to_owned())?
        .map_err(|error| format!("could not run policy preview: {error}"))?;
    if !preview.status.success() {
        return Err(format!("policy preview exited {}", preview.status).into());
    }
    match enabled(&preview.stdout) {
        Some(true) => return Ok(false),
        Some(false) => {}
        None => return Err("policy preview did not report whether policies are enabled".into()),
    }
    // The command writes the confirmation hint while waiting for the daemon.
    // Inherit stderr so the operator can see the hint before answering.
    let enable = command("enable")
        .stderr(Stdio::inherit())
        .output()
        .await
        .map_err(|error| format!("could not run policy enable: {error}"))?;
    match enable.status.code() {
        Some(0) if enabled(&enable.stdout) == Some(true) => Ok(true),
        Some(1) => Err(PolicyFailure::Refused),
        Some(2) => Err("daemon transport failed".into()),
        Some(0 | 3) | None => Err("enable outcome unknown".into()),
        _ => Err(format!("policy enable exited {}", enable.status).into()),
    }
}

fn ready(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Value>(bytes)
        .is_ok_and(|status| status["result"]["connectorHome"]["admitted"] == true)
}

fn enabled(bytes: &[u8]) -> Option<bool> {
    serde_json::from_slice::<Value>(bytes).ok()?["enabled"].as_bool()
}

fn shell_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    if path
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte))
    {
        path.into_owned()
    } else if cfg!(windows) {
        format!("\"{path}\"")
    } else {
        format!("'{}'", path.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shape of `ck-plexus-admin connector-home status` output, with invented names.
    const ADMITTED: &str = r#"{"result":{"role":"connector_home","connectorHome":{"holder_name":"test-operator","state":"held","admitted":true}}}"#;

    #[test]
    fn plexus_readiness_requires_admitted_true() {
        assert!(ready(ADMITTED.as_bytes()));
        for status in [
            ADMITTED.replace("\"admitted\":true", "\"admitted\":false"),
            ADMITTED.replace("\"admitted\":true", "\"admitted\":\"true\""),
            ADMITTED.replace(",\"admitted\":true", ""),
            ADMITTED.replace("\"admitted\":true", "\"partiallyAdmitted\":true"),
            r#"{"connectorHome":{"admitted":true}}"#.into(),
            "not JSON".into(),
        ] {
            assert!(!ready(status.as_bytes()), "{status}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn plexus_policy_waits_then_previews_and_only_enables_when_off() {
        use std::{fs, os::unix::fs::PermissionsExt};
        let scratch = cortexkit_test_support::ScratchDir::new("plexus-policy");
        let root = scratch.path();
        let connection = root.join("daemon connection.json");
        let admin = root.join("ck-plexus-admin");
        let probe = root.join("probed");
        let enabled_marker = root.join("enabled");
        let mode = root.join("mode");
        let script = |path, content| {
            fs::write(path, content).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        };
        // The first status probe answers "not admitted yet"; later ones admit.
        script(&admin, format!("#!/bin/sh\nif [ \"$1 $2 $3\" = \"connector-home status --subc\" ] && [ \"$4\" = {conn} ]; then if [ ! -f {probe} ]; then touch {probe}; echo '{NOT}'; else echo '{ADMITTED}'; fi; exit 0; fi\n[ \"$1\" = agent-policy ] && [ \"$3\" = --subc ] && [ \"$4\" = {conn} ] || exit 64\ncase \"$2\" in\npreview) if [ -f {enabled} ]; then echo '{{\"enabled\":true}}'; else echo '{{\"enabled\":false}}'; fi ;;\nenable) if [ -f {mode} ]; then exit $(cat {mode}); fi; touch {enabled}; echo '{{\"enabled\":true}}' ;;\n*) exit 64 ;;\nesac\n", conn = shell_path(&connection), probe = shell_path(&probe), NOT = ADMITTED.replace("\"admitted\":true", "\"admitted\":false"), enabled = shell_path(&enabled_marker), mode = shell_path(&mode)));

        assert!(enable_when_ready(&admin, &connection).await.unwrap());
        assert!(probe.is_file());
        assert!(enabled_marker.is_file());
        fs::write(&mode, "64").unwrap();
        assert!(!enable_when_ready(&admin, &connection).await.unwrap());
        fs::remove_file(&enabled_marker).unwrap();
        fs::write(&mode, "1").unwrap();
        assert!(matches!(
            enable_when_ready(&admin, &connection).await,
            Err(PolicyFailure::Refused)
        ));
        for code in ["2", "3"] {
            fs::write(&mode, code).unwrap();
            assert!(matches!(
                enable_when_ready(&admin, &connection).await,
                Err(PolicyFailure::Unknown(_))
            ));
        }
        assert!(!enabled_marker.exists());
    }
}
