use std::path::Path;

use serde_json::json;

use super::{config, runtime::CommandRunner};

pub(super) const GRANT_STEP: &str = "Let the message module use this computer's message-plane keys";
pub(super) const HUB_STEP: &str = "Install an idle message hub entry";

/// Individual signing keys require the operator's grants, not a module's
/// declaration. The vault CLI owns idempotence, including on older vaults.
pub(super) fn grant_keys(
    auth: &Path,
    runner: &mut dyn CommandRunner,
) -> Result<(bool, String), String> {
    if !auth.is_absolute() {
        return Err("refusal: the vault CLI must have an absolute installed path".into());
    }
    let mut selectors = Vec::new();
    for key in [
        "signing:bus-leaf:1",
        "signing:bus-hub-operator:1",
        "signing:bus-hub-account:1",
        "signing:bus-hub-tls:1",
    ] {
        for operation in ["read", "sign"] {
            selectors.push(("exact", key, operation));
        }
    }
    selectors.push(("audience", "cortexkit-hub", "account_token"));
    let mut changed = false;
    let mut lines = Vec::new();
    for (kind, selector, operation) in selectors {
        let args = [
            "grant",
            "--principal",
            "reserved:ckbus",
            "--selector-kind",
            kind,
            "--selector",
            selector,
            "--operation",
            operation,
        ]
        .map(str::to_string);
        let rerun = format!("\"{}\" {}", auth.display(), args.join(" "));
        let result = runner
            .run(&auth.to_string_lossy(), &args)
            .map_err(|error| format!("{GRANT_STEP} failed: {error}\nRerun: {rerun}"))?;
        if !result.success {
            return Err(format!(
                "{GRANT_STEP} failed:\n{}\nRerun: {rerun}",
                result.stderr.trim()
            ));
        }
        let output = result.stdout.trim();
        changed |= output != "already granted (no change)";
        lines.push(output.to_string());
    }
    let detail = if changed {
        format!("{}\nMessage-plane grants applied", lines.join("\n"))
    } else {
        "Message-plane grants: nothing changed (all nine already granted)".into()
    };
    Ok((changed, detail))
}

pub(super) fn wait_for_vault(
    ck: &Path,
    connection: Option<&Path>,
    runner: &mut dyn CommandRunner,
    timeout: std::time::Duration,
) -> Result<(), String> {
    // CLI integration fixtures have no daemon. Use the same readiness stub as
    // setup's other control calls, but still execute every grant in the fake CLI.
    #[cfg(feature = "test-support")]
    if std::env::var_os("CK_TEST_SETUP_CONTROL_OK").is_some() {
        return Ok(());
    }
    let mut args = Vec::new();
    if let Some(connection) = connection {
        args.extend(["--subc".into(), connection.to_string_lossy().into_owned()]);
    }
    args.extend(["--json", "module", "status", "claustrum"].map(str::to_string));
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let live = runner
            .run(&ck.to_string_lossy(), &args)
            .ok()
            .filter(|r| r.success)
            .and_then(|r| serde_json::from_str::<serde_json::Value>(&r.stdout).ok())
            .is_some_and(|v| v["module"]["live"] == true);
        if live {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err("The vault is not running. Run ck module start claustrum, then rerun ck setup or ck upgrade to grant the message-plane keys.".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// Keep the hub process enabled because supervisor.set_enabled does not persist
/// across daemon restarts. It idles until ck-bus says this computer is the hub,
/// so no module needs to edit the daemon configuration when the hub is chosen.
/// The config and JetStream store share the nats-hub data root. An existing
/// entry always belongs to the operator.
pub(super) fn plan_hub(
    config_path: &Path,
    binary_home: &Path,
    data_dir: &Path,
    connection: &Path,
) -> Result<Option<config::ConfigChange>, String> {
    config::plan_missing_value(
        config_path,
        "modules.nats-hub",
        json!({
            "program": binary_home.join(if cfg!(windows) { "ck-nats-server.exe" } else { "ck-nats-server" }),
            "args": ["-c", data_dir.join("nats-hub").join("server.conf"), "--subc", connection],
            "protocol": "subc",
            "registration_channel": true,
            "reserved": true,
            "enabled": true,
        }),
    )
}

pub(super) fn rollback_hub(change: &config::ConfigChange) -> Result<bool, String> {
    if std::fs::metadata(&change.path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
        return Ok(false);
    }
    let (_, current) = subc_jsonc::read_document(&change.path)?;
    let written: serde_json::Value =
        serde_json::from_str(&subc_jsonc::jsonc_to_json(&change.after).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    // Once the operator has changed the placeholder, its program and flags
    // must stay together rather than rolling back individual unchanged leaves.
    if current["modules"]["nats-hub"] != written["modules"]["nats-hub"] {
        return Ok(false);
    }
    config::rollback_change(change)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use cortexkit_test_support::ScratchDir;
    use subc_jsonc::read_document;

    use super::*;

    #[test]
    fn message_plane_waits_for_a_live_vault_before_granting() {
        struct Status(bool);
        impl CommandRunner for Status {
            fn run(
                &mut self,
                _: &str,
                args: &[String],
            ) -> Result<super::super::runtime::CommandResult, String> {
                assert_eq!(
                    args,
                    [
                        "--subc",
                        "isolated-connection",
                        "--json",
                        "module",
                        "status",
                        "claustrum"
                    ]
                );
                Ok(super::super::runtime::CommandResult {
                    success: true,
                    stdout: json!({"module":{"live":self.0}}).to_string(),
                    stderr: String::new(),
                })
            }
        }
        let error = wait_for_vault(
            Path::new("isolated-ck"),
            Some(Path::new("isolated-connection")),
            &mut Status(false),
            std::time::Duration::ZERO,
        )
        .unwrap_err();
        assert!(error.contains("Run ck module start claustrum"));
        wait_for_vault(
            Path::new("isolated-ck"),
            Some(Path::new("isolated-connection")),
            &mut Status(true),
            std::time::Duration::ZERO,
        )
        .unwrap();
    }

    #[cfg(unix)]
    fn fake_vault(root: &Path, fail: bool) -> std::path::PathBuf {
        let auth = root.join("vault cli");
        let log = root.join("calls");
        let seen = root.join("grants");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\n\
             if [ '{fail}' = true ] && [ \"$7\" = signing:bus-hub-operator:1 ]; then\n\
               echo 'vault refused the operator grant' >&2; exit 7\n\
             fi\n\
             if [ -f '{seen}' ] && /usr/bin/grep -Fxq -- \"$*\" '{seen}'; then\n\
               echo 'already granted (no change)'\n\
             else\n\
               printf '%s\\n' \"$*\" >> '{seen}'\n\
               echo \"granted $*\"\n\
             fi\n",
            log = log.display(),
            seen = seen.display(),
        );
        super::super::test_exec::write_executable(&auth, script.as_bytes());
        auth
    }

    #[cfg(unix)]
    #[test]
    fn message_plane_grants_run_in_order_and_repeat_without_changes() {
        let root = ScratchDir::new("message-grants-repeat");
        let auth = fake_vault(&root, false);
        let mut runner = super::super::runtime::SystemCommandRunner;
        let first = grant_keys(&auth, &mut runner).unwrap();
        assert!(first.0);
        assert!(first.1.contains("granted grant"));
        let expected = [
            "grant --principal reserved:ckbus --selector-kind exact --selector signing:bus-leaf:1 --operation read",
            "grant --principal reserved:ckbus --selector-kind exact --selector signing:bus-leaf:1 --operation sign",
            "grant --principal reserved:ckbus --selector-kind exact --selector signing:bus-hub-operator:1 --operation read",
            "grant --principal reserved:ckbus --selector-kind exact --selector signing:bus-hub-operator:1 --operation sign",
            "grant --principal reserved:ckbus --selector-kind exact --selector signing:bus-hub-account:1 --operation read",
            "grant --principal reserved:ckbus --selector-kind exact --selector signing:bus-hub-account:1 --operation sign",
            "grant --principal reserved:ckbus --selector-kind exact --selector signing:bus-hub-tls:1 --operation read",
            "grant --principal reserved:ckbus --selector-kind exact --selector signing:bus-hub-tls:1 --operation sign",
            "grant --principal reserved:ckbus --selector-kind audience --selector cortexkit-hub --operation account_token",
        ];
        let calls = fs::read_to_string(root.join("calls")).unwrap();
        assert_eq!(calls.lines().collect::<Vec<_>>(), expected);
        let grants = fs::read(root.join("grants")).unwrap();
        let second = grant_keys(&auth, &mut runner).unwrap();
        assert!(!second.0);
        assert!(second.1.contains("nothing changed"), "{second:?}");
        assert_eq!(fs::read(root.join("grants")).unwrap(), grants);
        assert_eq!(
            fs::read_to_string(root.join("calls")).unwrap(),
            calls.repeat(2)
        );
    }

    #[cfg(unix)]
    #[test]
    fn message_plane_grant_failure_stops_with_stderr_and_rerun_command() {
        let root = ScratchDir::new("message-grants-failure");
        let auth = fake_vault(&root, true);
        let error = grant_keys(&auth, &mut super::super::runtime::SystemCommandRunner).unwrap_err();
        assert!(
            error.contains("vault refused the operator grant"),
            "{error}"
        );
        assert!(
            error.contains(&format!("Rerun: \"{}\" grant", auth.display())),
            "{error}"
        );
        assert!(
            error.contains("--selector signing:bus-hub-operator:1 --operation read"),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(root.join("calls"))
                .unwrap()
                .lines()
                .count(),
            3
        );
    }

    #[test]
    fn message_plane_hub_is_added_once_and_existing_entry_is_never_changed() {
        let root = ScratchDir::new("message-hub-add");
        let path = root.join("subc.jsonc");
        let before = "{\n  // operator comment\n  \"modules\": {}\n}\n";
        fs::write(&path, before).unwrap();
        let home = root.join("bin");
        let connection = root.join("connection.json");
        let change = plan_hub(&path, &home, &root, &connection).unwrap().unwrap();
        config::apply(&change).unwrap();
        let (text, doc) = read_document(&path).unwrap();
        assert!(text.contains("// operator comment"));
        assert_eq!(
            doc["modules"]["nats-hub"],
            json!({
                "program": home.join(if cfg!(windows) { "ck-nats-server.exe" } else { "ck-nats-server" }),
                "args": ["-c", root.join("nats-hub").join("server.conf"), "--subc", connection],
                "protocol": "subc", "registration_channel": true, "reserved": true, "enabled": true,
            })
        );
        assert!(plan_hub(&path, &home, &root, &connection)
            .unwrap()
            .is_none());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);

        let custom =
            "{\"modules\":{\"nats-hub\":{\"enabled\":false,\"program\":\"operator-hub\"}}}";
        fs::write(&path, custom).unwrap();
        assert!(plan_hub(&path, &home, &root, &connection)
            .unwrap()
            .is_none());
        assert_eq!(fs::read_to_string(&path).unwrap(), custom);
    }

    #[test]
    fn message_plane_hub_rollback_removes_only_the_entry_this_run_added() {
        let root = ScratchDir::new("message-hub-rollback");
        let path = root.join("subc.jsonc");
        let before = "{\"modules\":{\"keep\":{\"program\":\"keep\"}}}\n";
        fs::write(&path, before).unwrap();
        let change = plan_hub(&path, &root.join("bin"), &root, &root.join("conn"))
            .unwrap()
            .unwrap();
        config::apply(&change).unwrap();
        rollback_hub(&change).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), before);

        config::apply(&change).unwrap();
        let mut edited = change.after.clone();
        subc_jsonc::set_member(
            &mut edited,
            &["modules", "nats-hub", "enabled"],
            &json!(false),
        )
        .unwrap();
        fs::write(&path, &edited).unwrap();
        assert!(!rollback_hub(&change).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), edited);
        fs::remove_file(&path).unwrap();
        assert!(!rollback_hub(&change).unwrap());
    }
}
