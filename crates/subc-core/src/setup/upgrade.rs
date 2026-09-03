use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use subc_transport::connection_file;

use super::{
    inventory::Inventory,
    model::{AlphaTarget, UpgradeObserved, UpgradeState, UpgradeTarget},
    release_index::ReleaseIndex,
    self_update,
    update_cache::UpdateMetadata,
    update_check::{observed_from_metadata, InstalledBinary},
    upgrade_assets::{
        prepare_upgrade_asset, sha256_file, PreparedUpgradeAsset, ReleaseUpgradeAssetFetcher,
    },
    upgrade_executor::{RollbackDecision, UpgradeExecutionBackend, UpgradeExecutionReport},
    upgrade_verification::{
        destination_inode, expected_post_activation, verify_post_activation, VerificationEvidence,
    },
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonCatalogBuild {
    pub pid: u32,
    pub version: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedUpgradeTarget {
    pub target: UpgradeTarget,
    pub destination: PathBuf,
    /// Display-only version text from the binary or daemon catalog.
    pub installed_version: String,
    /// Archive digest recorded at placement. Currency is undecidable without it.
    pub installed_archive_sha256: Option<String>,
}

/// Load the ownership record from the per-user data directory before discovery.
/// The record, not PATH or a configuration file, decides which destinations an
/// upgrade is allowed to replace.
pub fn discover_current_upgrade_targets(
    executable: &Path,
    daemon_catalog: Option<&DaemonCatalogBuild>,
) -> Result<Vec<ManagedUpgradeTarget>, String> {
    let inventory = load_current_inventory()?;
    discover_managed_upgrade_targets(&inventory, executable, daemon_catalog)
}

/// Reads only inventory evidence for the dashboard. It avoids probing binaries:
/// a bare `ck` must not assume every managed sibling shares its own crate version.
pub fn dashboard_installed_binaries() -> Result<BTreeMap<String, InstalledBinary>, String> {
    let inventory = load_current_inventory()?;
    let mut installed = BTreeMap::new();
    for target in UpgradeTarget::ORDERED {
        let path = ["managed-binary", "binary-placement"]
            .into_iter()
            .flat_map(|kind| inventory.paths_for_kind(kind))
            .find(|path| file_name_matches(path, target));
        let Some(path) = path else {
            continue;
        };
        let version =
            inventory_string(&inventory, &path, "version").unwrap_or_else(|| "unknown".to_string());
        let sha256 = inventory_string(&inventory, &path, "sha256");
        let archive_sha256 = inventory_string(&inventory, &path, "archive_sha256");
        installed.insert(
            target.label().to_string(),
            InstalledBinary {
                version,
                sha256,
                archive_sha256,
            },
        );
    }
    Ok(installed)
}

fn inventory_string(inventory: &Inventory, path: &Path, key: &str) -> Option<String> {
    ["managed-binary", "binary-placement"]
        .into_iter()
        .filter_map(|kind| inventory.entry_for_path(kind, path))
        .find_map(|entry| {
            entry
                .get(key)
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
}

fn load_current_inventory() -> Result<Inventory, String> {
    let data_dir = if cfg!(windows) {
        env::var_os("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .map(|path| path.join("cortexkit"))
            .ok_or_else(|| {
                "LOCALAPPDATA is unavailable for managed upgrade discovery".to_string()
            })?
    } else if let Some(data_home) = env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        PathBuf::from(data_home).join("cortexkit")
    } else {
        env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                "the user home directory is unavailable for managed upgrade discovery".to_string()
            })?
            .join(".local")
            .join("share")
            .join("cortexkit")
    };
    Inventory::load(
        data_dir.join("installer-manifest.json"),
        super::model::PlatformObservation::current()
            .to_string()
            .as_str(),
    )
}

/// Discover only inventory-owned targets. MC is intentionally absent because it
/// has no alpha archive; an MC data directory or configuration never creates an
/// upgrade target.
pub fn discover_managed_upgrade_targets(
    inventory: &Inventory,
    executable: &Path,
    daemon_catalog: Option<&DaemonCatalogBuild>,
) -> Result<Vec<ManagedUpgradeTarget>, String> {
    let mut owned = inventory.paths_for_kind("managed-binary");
    owned.extend(inventory.paths_for_kind("binary-placement"));
    let executable = canonical_or_original(executable);
    let mut targets = Vec::new();
    for target in UpgradeTarget::ORDERED {
        let destination = match target {
            UpgradeTarget::Ck => owned
                .iter()
                .find(|path| canonical_or_original(path) == executable)
                .cloned(),
            _ => owned
                .iter()
                .find(|path| file_name_matches(path, target))
                .cloned(),
        };
        let Some(destination) = destination else {
            continue;
        };
        if !destination.is_file() {
            return Err(format!(
                "refusal: inventory-owned {target} destination is missing: {}",
                destination.display()
            ));
        }
        let installed_version = match target {
            UpgradeTarget::Daemon => daemon_catalog
                .ok_or_else(|| {
                    "refusal: daemon catalog build information is unavailable for inventory-owned ck-subc"
                        .to_string()
                })?
                .version
                .clone(),
            _ => binary_version(&destination)?,
        };
        let installed_archive_sha256 = inventory_string(inventory, &destination, "archive_sha256");
        targets.push(ManagedUpgradeTarget {
            target,
            destination,
            installed_version,
            installed_archive_sha256,
        });
    }
    Ok(targets)
}

/// Combines inventory and running-version evidence with the release check.
pub fn observed_upgrade_targets(
    metadata: &UpdateMetadata,
    discovered: &[ManagedUpgradeTarget],
) -> UpgradeObserved {
    let installed = discovered
        .iter()
        .map(|item| {
            (
                item.target.label().to_string(),
                InstalledBinary {
                    version: item.installed_version.clone(),
                    sha256: None,
                    archive_sha256: item.installed_archive_sha256.clone(),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut observed = observed_from_metadata(metadata, &installed);
    for target in UpgradeTarget::ORDERED {
        if !discovered.iter().any(|item| item.target == target) {
            observed
                .targets
                .insert(target.label().to_string(), UpgradeState::NotInstalled);
        }
    }
    observed
}

/// The concrete command backend keeps mutation at the inventory-owned
/// destinations while its control calls go through `ck`'s established command
/// surface. Daemon activation deliberately uses the OS service manager rather
/// than a supervisor restart request.
pub struct SystemUpgradeBackend {
    platform: AlphaTarget,
    targets: BTreeMap<String, ManagedUpgradeTarget>,
    executable: PathBuf,
    subc: Option<PathBuf>,
    assets: ReleaseUpgradeAssetFetcher,
    inventory: Inventory,
    prepared: BTreeMap<String, PreparedUpgradeAsset>,
    rollback_paths: BTreeMap<String, PathBuf>,
    rollback_archive_sha256: BTreeMap<String, Option<String>>,
    expected_versions: BTreeMap<String, String>,
}

impl SystemUpgradeBackend {
    pub fn new(
        executable: impl Into<PathBuf>,
        subc: Option<PathBuf>,
        targets: Vec<ManagedUpgradeTarget>,
        index: ReleaseIndex,
    ) -> Result<Self, String> {
        let platform = match super::model::PlatformObservation::current() {
            super::model::PlatformObservation::Supported(platform) => platform,
            super::model::PlatformObservation::Unsupported(host) => {
                let supported: Vec<&str> = AlphaTarget::ALL.iter().map(|t| t.label()).collect();
                return Err(format!(
                    "unsupported-platform: {host} (alpha supports: {})",
                    supported.join(", ")
                ));
            }
        };
        let inventory = load_current_inventory()?;
        let expected_versions = targets
            .iter()
            .map(|item| {
                (
                    item.target.label().to_string(),
                    item.installed_version.clone(),
                )
            })
            .collect();
        Ok(Self {
            platform,
            targets: targets
                .into_iter()
                .map(|item| (item.target.label().to_string(), item))
                .collect(),
            executable: executable.into(),
            subc,
            assets: ReleaseUpgradeAssetFetcher::from_index(index),
            inventory,
            prepared: BTreeMap::new(),
            rollback_paths: BTreeMap::new(),
            rollback_archive_sha256: BTreeMap::new(),
            expected_versions,
        })
    }

    pub fn set_expected_version(&mut self, target: UpgradeTarget, version: String) {
        self.expected_versions
            .insert(target.label().to_string(), version);
    }

    fn target(&self, target: UpgradeTarget) -> Result<&ManagedUpgradeTarget, String> {
        self.targets
            .get(target.label())
            .ok_or_else(|| format!("refusal: {target} is not a managed inventory target"))
    }

    fn target_mutable_paths(&self, target: UpgradeTarget) -> Result<(PathBuf, PathBuf), String> {
        let destination = self.target(target)?.destination.clone();
        let rollback = destination.with_extension(format!(
            "{}.rollback",
            destination
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or("ck-upgrade")
        ));
        Ok((destination, rollback))
    }

    fn ck_command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        if let Some(subc) = &self.subc {
            command.arg("--subc").arg(subc);
        }
        command
    }

    fn run_ck(&self, args: &[&str]) -> Result<String, String> {
        let output = self
            .ck_command()
            .args(args)
            .output()
            .map_err(|error| format!("could not run ck {}: {error}", args.join(" ")))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(format!(
                "ck {} exited {}: {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    fn module_ready(&self, target: UpgradeTarget) -> Result<bool, String> {
        let output = self.run_ck(&["--json", "module", "status", target.label()])?;
        let value: Value = serde_json::from_str(&output)
            .map_err(|error| format!("invalid module status JSON for {target}: {error}"))?;
        let live = value
            .get("module")
            .and_then(|module| module.get("live"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let healthy = value
            .get("health")
            .and_then(|health| health.get("status"))
            .and_then(Value::as_str)
            .is_some_and(|status| matches!(status, "ok" | "healthy"));
        Ok(live && healthy)
    }

    fn wait_until<F>(&self, timeout: Duration, mut ready: F) -> Result<(), String>
    where
        F: FnMut() -> Result<bool, String>,
    {
        let deadline = Instant::now() + timeout;
        loop {
            if ready()? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "completion did not become healthy within {} seconds",
                    timeout.as_secs()
                ));
            }
            thread::sleep(Duration::from_millis(250));
        }
    }

    fn daemon_ready(&self) -> Result<bool, String> {
        let Some(subc) = &self.subc else {
            return Err(
                "no daemon connection file was supplied for service verification".to_string(),
            );
        };
        let connection = connection_file::read_for_client(subc)
            .map_err(|error| format!("could not read daemon connection after restart: {error}"))?;
        let expected = self
            .expected_versions
            .get(UpgradeTarget::Daemon.label())
            .map(String::as_str)
            .unwrap_or_default();
        if connection.pid == 0 || connection.daemon_ver != expected {
            return Ok(false);
        }
        self.run_ck(&["daemon"])?;
        Ok(true)
    }

    fn module_provenance(&self, target: UpgradeTarget) -> Result<(Option<u32>, bool), String> {
        let output = self.run_ck(&["--json", "provenance", target.label()])?;
        let value: Value = serde_json::from_str(&output)
            .map_err(|error| format!("invalid provenance JSON for {target}: {error}"))?;
        let module = value
            .get("modules")
            .and_then(Value::as_array)
            .and_then(|modules| modules.first())
            .ok_or_else(|| format!("provenance response omitted {target}"))?;
        let pid = module
            .get("daemon_observed")
            .and_then(|observed| observed.get("pid"))
            .and_then(Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok());
        let matches_destination = module
            .get("daemon_observed")
            .and_then(|observed| observed.get("running_image"))
            .and_then(|image| image.get("status"))
            .and_then(Value::as_str)
            == Some("match");
        Ok((pid, matches_destination))
    }

    fn record_replacement_digest(
        &mut self,
        target: UpgradeTarget,
        destination: &Path,
        archive_sha256: Option<&str>,
    ) -> Result<(), String> {
        let digest = sha256_file(destination)?;
        // A replacement changes the inode, so retain the extracted-binary
        // ownership digest, the archive digest currency compares to the index,
        // and the self-reported version for the next dashboard. Version text is
        // not an update decision.
        let version = binary_version(destination).ok();
        let kinds = ["managed-binary", "binary-placement"]
            .into_iter()
            .filter(|kind| self.inventory.owns_path(kind, destination))
            .collect::<Vec<_>>();
        if kinds.is_empty() {
            return Err(format!(
                "inventory no longer owns {target} destination {}; refusing to record replacement",
                destination.display()
            ));
        }
        for kind in kinds {
            self.inventory
                .update_owned_string(kind, destination, "sha256", digest.clone())?;
            match archive_sha256 {
                Some(archive) => self.inventory.update_owned_string(
                    kind,
                    destination,
                    "archive_sha256",
                    archive.to_string(),
                )?,
                None => self
                    .inventory
                    .remove_owned_string(kind, destination, "archive_sha256")?,
            }
            if let Some(version) = &version {
                self.inventory.update_owned_string(
                    kind,
                    destination,
                    "version",
                    version.clone(),
                )?;
            }
        }
        self.inventory.save()
    }

    fn expected_version(&self, target: UpgradeTarget) -> Result<&str, String> {
        self.expected_versions
            .get(target.label())
            .map(String::as_str)
            .ok_or_else(|| format!("no expected release version recorded for {target}"))
    }
}

impl UpgradeExecutionBackend for SystemUpgradeBackend {
    fn download_and_verify(&mut self, target: UpgradeTarget) -> Result<String, String> {
        let prepared = prepare_upgrade_asset(&mut self.assets, target, self.platform)
            .map_err(|error| error.to_string())?;
        let detail = format!("archive={} SHA-256=verified", prepared.names.archive);
        self.prepared.insert(target.label().to_string(), prepared);
        Ok(detail)
    }

    fn create_rollback_copy(&mut self, target: UpgradeTarget) -> Result<String, String> {
        let (destination, rollback) = self.target_mutable_paths(target)?;
        let prior_inode = destination_inode(&destination)?;
        fs::copy(&destination, &rollback).map_err(|error| {
            format!(
                "could not create rollback copy {} from {}: {error}",
                rollback.display(),
                destination.display()
            )
        })?;
        self.rollback_paths
            .insert(target.label().to_string(), rollback);
        self.rollback_archive_sha256.insert(
            target.label().to_string(),
            inventory_string(&self.inventory, &destination, "archive_sha256"),
        );
        Ok(format!("rollback copy created; prior inode={prior_inode}"))
    }

    fn replace_destination(&mut self, target: UpgradeTarget) -> Result<String, String> {
        let (destination, _) = self.target_mutable_paths(target)?;
        let prepared = self
            .prepared
            .remove(target.label())
            .ok_or_else(|| format!("no verified candidate was prepared for {target}"))?;
        if target == UpgradeTarget::Ck {
            let result = self_update::replace_verified_candidate(
                &destination,
                &prepared.candidate,
                &prepared.archive_sha256,
                &mut self.inventory,
            );
            prepared.cleanup();
            return result.map(|evidence| evidence.to_string());
        }

        let parent = destination.parent().ok_or_else(|| {
            format!(
                "managed destination {} has no parent",
                destination.display()
            )
        })?;
        let temporary = parent.join(format!(".{}.upgrade", target.label()));
        fs::copy(&prepared.candidate, &temporary).map_err(|error| {
            format!(
                "could not place verified candidate at {}: {error}",
                temporary.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755)).map_err(
                |error| format!("could not mark {} executable: {error}", temporary.display()),
            )?;
        }
        fs::rename(&temporary, &destination).map_err(|error| {
            format!(
                "could not replace managed destination {}: {error}",
                destination.display()
            )
        })?;
        let archive_sha256 = prepared.archive_sha256.clone();
        prepared.cleanup();
        self.record_replacement_digest(target, &destination, Some(&archive_sha256))?;
        Ok(format!(
            "destination replaced; inode={}",
            destination_inode(&destination)?
        ))
    }

    fn warm_execute(&mut self, target: UpgradeTarget) -> Result<String, String> {
        let destination = &self.target(target)?.destination;
        let output = Command::new(destination)
            .arg("--version")
            .output()
            .map_err(|error| {
                format!("could not warm-execute {}: {error}", destination.display())
            })?;
        if !output.status.success() {
            return Err(format!(
                "destination inode {} exited {}: {}",
                destination_inode(destination)?,
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(format!(
            "destination inode={} executed successfully ({})",
            destination_inode(destination)?,
            version_from_output(&output.stdout)?
        ))
    }

    fn initiate_module_restart(
        &mut self,
        target: UpgradeTarget,
        drain_timeout: Duration,
    ) -> Result<String, String> {
        let drain = drain_timeout.as_millis().to_string();
        let output = self.run_ck(&["module", "restart", target.label(), "--drain-ms", &drain])?;
        if !output.contains("restart") {
            return Err(format!(
                "restart command returned no initiation acknowledgement for {target}"
            ));
        }
        Ok(format!(
            "initiation acknowledged; drain={}s",
            drain_timeout.as_secs()
        ))
    }

    fn poll_module_restart_completion(
        &mut self,
        target: UpgradeTarget,
        drain_timeout: Duration,
    ) -> Result<String, String> {
        self.wait_until(drain_timeout, || self.module_ready(target))?;
        Ok(format!(
            "module is live and healthy after {}s drain",
            drain_timeout.as_secs()
        ))
    }

    fn restart_daemon_via_service_manager(
        &mut self,
        drain_timeout: Duration,
    ) -> Result<String, String> {
        let detail = super::runtime::restart_via_service_manager()?;
        Ok(format!(
            "{detail}; drain budget={}s",
            drain_timeout.as_secs()
        ))
    }

    fn poll_daemon_service_ready(&mut self, drain_timeout: Duration) -> Result<String, String> {
        self.wait_until(drain_timeout, || self.daemon_ready())?;
        Ok(format!(
            "daemon service is live and healthy after {}s drain",
            drain_timeout.as_secs()
        ))
    }

    fn post_verify(&mut self, target: UpgradeTarget) -> Result<String, String> {
        let destination = &self.target(target)?.destination;
        let (pid, healthy, running_image_matches_destination, version) = match target {
            UpgradeTarget::SubcMcp | UpgradeTarget::Aft => {
                let (pid, running_image_matches_destination) = self.module_provenance(target)?;
                let healthy = self.module_ready(target)?;
                (
                    pid,
                    healthy,
                    running_image_matches_destination,
                    binary_version(destination)?,
                )
            }
            UpgradeTarget::Daemon => {
                let subc = self.subc.as_ref().ok_or_else(|| {
                    "no daemon connection file was supplied for verification".to_string()
                })?;
                let info = connection_file::read_for_client(subc)
                    .map_err(|error| format!("could not read daemon connection: {error}"))?;
                self.run_ck(&["daemon"])?;
                (Some(info.pid), true, true, info.daemon_ver)
            }
            UpgradeTarget::Ck => (Some(process_id()), true, true, binary_version(destination)?),
        };
        // Module sibling crates may report a version unrelated to their source
        // release. Their digest, destination inode, liveness, and health are the
        // proof; preserve version text as evidence without making it a gate.
        let expected_version = match target {
            UpgradeTarget::SubcMcp | UpgradeTarget::Aft => version.clone(),
            UpgradeTarget::Daemon | UpgradeTarget::Ck => self.expected_version(target)?.to_string(),
        };
        let expectation = expected_post_activation(
            destination,
            expected_version,
            target != UpgradeTarget::Ck,
            target != UpgradeTarget::Ck,
        )?;
        let evidence = VerificationEvidence {
            pid,
            inode: destination_inode(destination)?,
            healthy,
            version,
            running_image_matches_destination,
        };
        verify_post_activation(&evidence, &expectation).map_err(|error| {
            format!(
                "{}: {error}",
                super::upgrade_verification::target_verification_label(target)
            )
        })
    }

    fn rollback_decision(&mut self, _target: UpgradeTarget) -> RollbackDecision {
        match env::var("CK_UPGRADE_ROLLBACK") {
            Ok(value) if matches!(value.as_str(), "accept" | "accepted" | "yes") => {
                RollbackDecision::Accepted
            }
            _ => RollbackDecision::Declined,
        }
    }

    fn rollback(&mut self, target: UpgradeTarget) -> Result<String, String> {
        let destination = self.target(target)?.destination.clone();
        let rollback = self
            .rollback_paths
            .get(target.label())
            .ok_or_else(|| format!("no rollback copy exists for {target}"))?;
        fs::copy(rollback, &destination).map_err(|error| {
            format!(
                "could not restore rollback copy {} to {}: {error}",
                rollback.display(),
                destination.display()
            )
        })?;
        let previous_archive = self
            .rollback_archive_sha256
            .remove(target.label())
            .flatten();
        self.record_replacement_digest(target, &destination, previous_archive.as_deref())?;
        Ok(format!(
            "accepted; restored prior inode={}",
            destination_inode(&destination)?
        ))
    }
}

pub fn render_execution_report(report: &UpgradeExecutionReport) {
    for evidence in &report.evidence {
        println!("{evidence}");
    }
}

pub fn binary_version(path: &Path) -> Result<String, String> {
    let output = Command::new(path)
        .arg("--version")
        .output()
        .map_err(|error| format!("could not run {} --version: {error}", path.display()))?;
    if !output.status.success() {
        return Err(format!(
            "refusal: {} --version exited {}: {}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    version_from_output(&output.stdout)
}

fn version_from_output(output: &[u8]) -> Result<String, String> {
    let output = String::from_utf8_lossy(output);
    output
        .split_whitespace()
        .map(|token| token.trim_start_matches('v'))
        .find(|token| {
            let mut parts = token.split('.');
            matches!(
                (parts.next(), parts.next(), parts.next()),
                (Some(major), Some(minor), Some(patch))
                    if major.chars().all(char::is_numeric)
                        && minor.chars().all(char::is_numeric)
                        && patch.chars().next().is_some_and(char::is_numeric)
            )
        })
        .map(|version| {
            version
                .chars()
                .take_while(|character| character.is_ascii_digit() || *character == '.')
                .collect()
        })
        .filter(|version: &String| !version.is_empty())
        .ok_or_else(|| format!("refusal: --version output had no semantic version: {output:?}"))
}

fn canonical_or_original(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn file_name_matches(path: &Path, target: UpgradeTarget) -> bool {
    let name = path.file_name().and_then(|name| name.to_str());
    name == Some(target.label()) || name == Some(&format!("{}.exe", target.label()))
}

fn process_id() -> u32 {
    std::process::id()
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use serde_json::{Map, Value};

    use super::*;
    #[cfg(unix)]
    use subc_core::test_support::TestTempDir;

    // Used only by the unix-gated tests below; gate it with them so windows
    // clippy under -D warnings does not read it as dead code.
    #[cfg(unix)]
    fn fixture_dir(name: &str) -> TestTempDir {
        TestTempDir::new(name)
    }

    #[cfg(unix)]
    fn version_binary(path: &Path, version: &str) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, format!("#!/bin/sh\necho 'binary {version}'\n")).expect("script");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("executable");
    }

    #[cfg(unix)]
    #[test]
    fn discovery_uses_inventory_and_version_outputs_and_excludes_mc() {
        let root = fixture_dir("discovery");
        let ck = root.join("ck");
        let mcp = root.join("ck-subc-mcp");
        let aft = root.join("ck-aft");
        let daemon = root.join("ck-subc");
        let mc = root.join("ck-mc");
        version_binary(&ck, "1.0.0");
        version_binary(&mcp, "1.1.0");
        version_binary(&aft, "1.2.0");
        version_binary(&daemon, "not-used");
        version_binary(&mc, "99.0.0");
        let mut inventory =
            Inventory::load(root.join("installer-manifest.json"), "linux-x64").expect("inventory");
        for path in [&ck, &mcp, &aft, &daemon, &mc] {
            inventory.record("managed-binary", path, Map::new());
        }
        inventory.record("binary-placement", &ck, Map::new());

        let targets = discover_managed_upgrade_targets(
            &inventory,
            &ck,
            Some(&DaemonCatalogBuild {
                pid: 44,
                version: "1.3.0".to_string(),
            }),
        )
        .expect("discover targets");
        assert_eq!(
            targets.iter().map(|item| item.target).collect::<Vec<_>>(),
            UpgradeTarget::ORDERED
        );
        assert_eq!(targets[0].installed_version, "1.1.0");
        assert_eq!(targets[1].installed_version, "1.2.0");
        assert_eq!(targets[2].installed_version, "1.3.0");
        assert_eq!(targets[3].installed_version, "1.0.0");
    }

    #[cfg(unix)]
    #[test]
    fn discovery_reads_archive_digest_for_currency_not_binary_digest() {
        let root = fixture_dir("currency-digest");
        let ck = root.join("ck");
        let mcp = root.join("ck-subc-mcp");
        version_binary(&ck, "1.0.0");
        version_binary(&mcp, "0.1.0");
        let mut inventory =
            Inventory::load(root.join("installer-manifest.json"), "linux-x64").expect("inventory");
        let mut fields = Map::new();
        fields.insert("sha256".to_string(), Value::String("ab".repeat(32)));
        fields.insert("archive_sha256".to_string(), Value::String("cd".repeat(32)));
        inventory.record("managed-binary", &mcp, fields);

        let targets = discover_managed_upgrade_targets(&inventory, &ck, None).expect("discover");
        let mcp_target = targets
            .iter()
            .find(|item| item.target == UpgradeTarget::SubcMcp)
            .expect("ck-subc-mcp");
        let archive = "cd".repeat(32);
        let binary = "ab".repeat(32);
        assert_eq!(
            mcp_target.installed_archive_sha256.as_deref(),
            Some(archive.as_str())
        );
        assert_ne!(
            mcp_target.installed_archive_sha256.as_deref(),
            Some(binary.as_str())
        );
    }

    #[test]
    fn missing_aft_archive_is_typed_release_incomplete() {
        let target = ManagedUpgradeTarget {
            target: UpgradeTarget::Aft,
            destination: PathBuf::from("/managed/ck-aft"),
            installed_version: "1.0.0".to_string(),
            installed_archive_sha256: Some("ab".repeat(32)),
        };
        let mut metadata = UpdateMetadata {
            format_version: super::super::update_cache::UPDATE_CACHE_FORMAT_VERSION,
            checked_at_unix_secs: 1,
            targets: BTreeMap::new(),
        };
        metadata.targets.insert(
            UpgradeTarget::Aft.label().to_string(),
            super::super::update_cache::CachedRelease {
                version: "2.0.0".to_string(),
                sha256: None,
            },
        );
        let observed = observed_upgrade_targets(&metadata, &[target]);
        assert!(matches!(
            observed.release(UpgradeTarget::Aft),
            super::super::model::ReleaseAvailability::Incomplete { .. }
        ));
    }
}
