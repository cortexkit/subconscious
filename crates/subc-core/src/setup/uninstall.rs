use std::{
    fs,
    path::{Path, PathBuf},
};

use super::{
    components::digest_file,
    inventory::Inventory,
    runtime::{self, CommandRunner, RuntimePaths, RuntimePlatform},
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UninstallReport {
    pub removed: Vec<PathBuf>,
    pub retained: Vec<String>,
    /// Removals that complete only after this process exits, each stated
    /// with the mechanism, so the operator is not left to wonder about a
    /// file that is still there when the command returns.
    pub deferred: Vec<String>,
}

/// Remove only paths named by the ownership inventory. Configuration and stores
/// are deliberately retention-only categories, even when setup created them.
pub fn uninstall<R: CommandRunner>(
    platform: RuntimePlatform,
    runtime_paths: &RuntimePaths,
    daemon_pid: Option<u32>,
    runner: &mut R,
    inventory: &mut Inventory,
    config_path: &Path,
    store_paths: &[PathBuf],
) -> Result<UninstallReport, String> {
    let mut report = UninstallReport::default();
    if inventory.owns_path("runtime-registration", &runtime_paths.definition) {
        runtime::deregister(platform, runtime_paths, daemon_pid, runner)?;
        inventory.remove_owned_path("runtime-registration", &runtime_paths.definition);
    }
    remove_owned_path(
        "runtime-definition",
        &runtime_paths.definition,
        inventory,
        &mut report,
    )?;

    for kind in ["managed-link", "managed-binary", "binary-placement"] {
        for path in inventory.paths_for_kind(kind) {
            remove_owned_path(kind, &path, inventory, &mut report)?;
        }
    }

    report
        .retained
        .push(format!("configuration: {}", config_path.display()));
    for store in store_paths {
        report.retained.push(format!("store: {}", store.display()));
    }
    Ok(report)
}

fn remove_owned_path(
    kind: &str,
    path: &Path,
    inventory: &mut Inventory,
    report: &mut UninstallReport,
) -> Result<(), String> {
    if !inventory.owns_path(kind, path) {
        return Ok(());
    }
    if path.exists() {
        if let Some(expected) = inventory
            .entry_for_path(kind, path)
            .and_then(|entry| entry.get("sha256"))
            .and_then(|digest| digest.as_str())
        {
            let actual = digest_file(path)?;
            if actual != expected {
                report.retained.push(format!(
                    "modified managed destination retained: {}",
                    path.display()
                ));
                return Ok(());
            }
        }
        if path.is_dir() {
            return Err(format!(
                "refusal: inventory-owned {} is a directory and will not be removed: {}",
                kind,
                path.display()
            ));
        }
        if cfg!(windows) && is_this_process_image(path) {
            let parked = retire_running_image(path)?;
            report.deferred.push(format!(
                "{}: Windows cannot delete a running executable; renamed to {} and deleted after this process exits",
                plain_path(path),
                plain_path(&parked)
            ));
            inventory.remove_owned_path(kind, path);
            return Ok(());
        }
        remove_file_when_released(path).map_err(|error| {
            format!(
                "could not remove inventory-owned {}: {error}",
                path.display()
            )
        })?;
        report.removed.push(path.to_path_buf());
    }
    inventory.remove_owned_path(kind, path);
    Ok(())
}

/// Whether `path` is the executable this process is running from. The
/// uninstall removes the managed `ck.exe`, and on Windows that is the very
/// image executing the removal.
fn is_this_process_image(path: &Path) -> bool {
    let Ok(this) = std::env::current_exe().and_then(fs::canonicalize) else {
        return false;
    };
    fs::canonicalize(path).map(|p| p == this).unwrap_or(false)
}

/// Removes the running executable the only way Windows allows: rename it out
/// from under its name now (permitted while it runs), and hand the delete to
/// a detached `cmd` that waits for this process to exit. The bin directory
/// stops resolving `ck` the moment the rename lands; the parked file carries
/// a name that explains itself if the deferred delete never runs.
fn retire_running_image(path: &Path) -> Result<PathBuf, String> {
    let parked = path.with_extension(format!("exe.uninstalled-{}", std::process::id()));
    fs::rename(path, &parked).map_err(|error| {
        format!(
            "could not rename the running executable {} aside for removal: {error}",
            path.display()
        )
    })?;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // cmd.exe rejects the `\\?\` extended-length form ("The specified
        // path is invalid"), so the deleter gets the plain drive path.
        let plain = plain_path(&parked);
        // The command line reaches cmd.exe verbatim (`raw_arg`): the default
        // argument quoting would wrap the whole script in quotes and escape
        // the path's, which cmd reads as a bad filename. DETACHED_PROCESS |
        // CREATE_NO_WINDOW: the deleter must outlive us and must not flash a
        // console.
        let script = format!("/C ping 127.0.0.1 -n 4 > nul & del /F /Q \"{plain}\"");
        let _ = std::process::Command::new("cmd.exe")
            .raw_arg(&script)
            .creation_flags(0x0000_0008 | 0x0800_0000)
            .spawn()
            .map_err(|error| {
                format!(
                    "could not schedule deletion of {}: {error}",
                    parked.display()
                )
            })?;
    }
    Ok(parked)
}

/// A path as an operator or cmd.exe reads it: inventory rows carry canonical
/// Windows paths with the `\\?\` extended-length prefix, which cmd.exe
/// refuses and no operator types.
fn plain_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
}

/// Removes a file, waiting out a process that is still releasing it. On
/// Windows an executable stays undeletable ("Access is denied") until the
/// process running it has fully exited, and the daemon was asked to stop
/// only moments earlier by `deregister`; the wait is bounded so a daemon
/// that never exits still produces the refusal, with the OS's words.
fn remove_file_when_released(path: &Path) -> std::io::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error)
                if cfg!(windows)
                    && error.kind() == std::io::ErrorKind::PermissionDenied
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Map;

    use super::*;
    use subc_core::test_support::TestTempDir;

    #[derive(Default)]
    struct SuccessfulRunner;
    impl CommandRunner for SuccessfulRunner {
        fn run(
            &mut self,
            _program: &str,
            _args: &[String],
        ) -> Result<super::super::runtime::CommandResult, String> {
            Ok(super::super::runtime::CommandResult {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    fn fixture_dir(name: &str) -> TestTempDir {
        TestTempDir::new(name)
    }

    #[test]
    fn uninstall_removes_only_inventory_paths_and_retains_configuration_and_stores() {
        let root = fixture_dir("owned-only");
        let managed = root.join("ck-subc");
        let unrelated = root.join("notes.txt");
        let config = root.join("subc.jsonc");
        let store = root.join("store.sqlite");
        fs::write(&managed, "managed").expect("managed binary");
        fs::write(&unrelated, "user data").expect("user data");
        fs::write(&config, "user config").expect("config");
        fs::write(&store, "user store").expect("store");
        let runtime_paths = RuntimePaths {
            definition: root.join("unit"),
            daemon: managed.clone(),
        };
        let mut inventory =
            Inventory::load(root.join("installer-manifest.json"), "linux-x64").expect("inventory");
        let mut fields = Map::new();
        fields.insert(
            "sha256".to_string(),
            serde_json::Value::String(digest_file(&managed).expect("digest")),
        );
        inventory.record("managed-binary", &managed, fields);

        let report = uninstall(
            RuntimePlatform::Linux,
            &runtime_paths,
            None,
            &mut SuccessfulRunner,
            &mut inventory,
            &config,
            std::slice::from_ref(&store),
        )
        .expect("uninstall");
        assert!(!managed.exists());
        assert!(unrelated.exists());
        assert!(config.exists());
        assert!(store.exists());
        assert!(report
            .retained
            .iter()
            .any(|line| line.contains("configuration")));
        assert!(report.retained.iter().any(|line| line.contains("store")));
    }

    #[test]
    fn uninstall_retains_modified_bytes_even_when_archive_digest_matches() {
        let root = fixture_dir("archive-digest-is-not-ownership");
        let managed = root.join("ck-subc");
        fs::write(&managed, "placed-bytes").expect("managed binary");
        let runtime_paths = RuntimePaths {
            definition: root.join("unit"),
            daemon: managed.clone(),
        };
        let mut inventory =
            Inventory::load(root.join("installer-manifest.json"), "linux-x64").expect("inventory");
        let on_disk = digest_file(&managed).expect("on-disk digest");
        let mut fields = Map::new();
        fields.insert(
            "sha256".to_string(),
            serde_json::Value::String("ff".repeat(32)),
        );
        fields.insert(
            "archive_sha256".to_string(),
            serde_json::Value::String(on_disk),
        );
        inventory.record("managed-binary", &managed, fields);

        let report = uninstall(
            RuntimePlatform::Linux,
            &runtime_paths,
            None,
            &mut SuccessfulRunner,
            &mut inventory,
            &root.join("subc.jsonc"),
            &[],
        )
        .expect("uninstall");

        assert!(
            managed.exists(),
            "ownership uses the extracted-binary digest; a matching archive digest must not delete changed bytes"
        );
        assert!(report
            .retained
            .iter()
            .any(|line| line.contains("modified managed destination retained")));
    }
}
