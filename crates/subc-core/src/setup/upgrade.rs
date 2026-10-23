use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use subc_transport::connection_file;

use super::{
    components::{installed_components, upgrade_roster},
    file_modes,
    inventory::Inventory,
    managed_replace,
    model::{AlphaTarget, Component, UpgradeObserved, UpgradeTarget},
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
    /// Connection metadata reports the daemon library, not the core release.
    pub version: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedUpgradeTarget {
    pub target: UpgradeTarget,
    pub destination: PathBuf,
    /// Binary-reported version; the core release also gates module compatibility.
    pub installed_version: String,
    /// Archive digest recorded at placement. Currency is undecidable without it.
    pub installed_archive_sha256: Option<String>,
}

#[derive(serde::Deserialize)]
struct CoreBuildInfo {
    core_version: String,
    build_git_sha: Option<String>,
    build_lock_digest: Option<String>,
}

impl CoreBuildInfo {
    fn has_identity(&self) -> bool {
        [(&self.build_git_sha, 40), (&self.build_lock_digest, 64)]
            .into_iter()
            .all(|(value, length)| {
                value.as_ref().is_some_and(|value| {
                    value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            })
    }

    fn matches_running(&self, running: &Value) -> bool {
        self.has_identity()
            && running.get("build_git_sha").and_then(Value::as_str) == self.build_git_sha.as_deref()
            && running.get("build_lock_digest").and_then(Value::as_str)
                == self.build_lock_digest.as_deref()
    }
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
    for target in upgrade_roster(installed_components(&inventory)) {
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

/// The managed data directory, resolved the way setup resolves it; the
/// inventory and the placed binaries both live under it.
fn load_current_inventory_root() -> Result<PathBuf, String> {
    if cfg!(windows) {
        return env::var_os("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .map(|path| path.join("cortexkit"))
            .ok_or_else(|| {
                "LOCALAPPDATA is unavailable for managed upgrade discovery".to_string()
            });
    }
    if let Some(data_home) = env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(data_home).join("cortexkit"));
    }
    Ok(env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            "the user home directory is unavailable for managed upgrade discovery".to_string()
        })?
        .join(".local")
        .join("share")
        .join("cortexkit"))
}

fn load_current_inventory() -> Result<Inventory, String> {
    Inventory::load(
        load_current_inventory_root()?.join("installer-manifest.json"),
        super::model::PlatformObservation::current()
            .to_string()
            .as_str(),
    )
}

fn tighten_upgrade_permissions(data_dir: &Path, inventory: &Inventory) -> Result<(), String> {
    file_modes::tighten_owned_directory(data_dir, 0o700)?;
    file_modes::tighten_owned_directory(&data_dir.join("bin"), 0o755)?;
    file_modes::tighten_owned_directory(&data_dir.join("cmd"), 0o755)?;
    file_modes::tighten_owned_directory(&data_dir.join("claustrum"), 0o700)?;
    file_modes::tighten_owned_directory(&data_dir.join("stores"), 0o700)?;
    for path in inventory.paths_for_kind("configuration") {
        if let Some(parent) = path.parent() {
            file_modes::tighten_owned_directory(parent, 0o700)?;
        }
    }
    for path in inventory
        .paths_for_kind("runtime-definition")
        .into_iter()
        .chain(inventory.paths_for_kind("runtime-registration"))
    {
        if let Some(parent) = path.parent() {
            file_modes::tighten_owned_directory(parent, 0o755)?;
        }
    }
    file_modes::tighten_owned_file(&data_dir.join("installer-manifest.json"), 0o644)?;
    file_modes::tighten_owned_file(&data_dir.join("claustrum/master.key"), 0o600)?;
    file_modes::tighten_inventory_files(inventory)
}

/// Discover only inventory-owned binaries belonging to installed components.
pub fn discover_managed_upgrade_targets(
    inventory: &Inventory,
    executable: &Path,
    _daemon_catalog: Option<&DaemonCatalogBuild>,
) -> Result<Vec<ManagedUpgradeTarget>, String> {
    let owned = super::components::owned_binary_paths(inventory);
    let executable = canonical_or_original(executable);
    let mut targets = Vec::new();
    for target in upgrade_roster(installed_components(inventory)) {
        let destination = if target.is_self_replacing() {
            owned
                .iter()
                .find(|path| canonical_or_original(path) == executable)
                .cloned()
        } else {
            owned
                .iter()
                .find(|path| file_name_matches(path, target))
                .cloned()
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
        let installed_version = {
            #[cfg(feature = "test-support")]
            if let Some(version) = test_installed_version(target) {
                version
            } else {
                binary_version(&destination)?
            }
            #[cfg(not(feature = "test-support"))]
            binary_version(&destination)?
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

#[cfg(feature = "test-support")]
/// `CK_TEST_<LABEL>_VERSION` for any roster binary, the label upper-cased
/// with its `ck-` prefix dropped and dashes as underscores: `ck-subc-mcp` →
/// `CK_TEST_SUBC_MCP_VERSION`, `ck` → `CK_TEST_CK_VERSION`. Derived rather
/// than listed so a binary the roster gains is coverable by the CLI fixtures
/// on Windows, where a shell-script fake cannot be executed for its version.
fn test_installed_version(target: UpgradeTarget) -> Option<String> {
    let label = target.label();
    let stem = label.strip_prefix("ck-").unwrap_or(label);
    let key = format!(
        "CK_TEST_{}_VERSION",
        stem.to_ascii_uppercase().replace('-', "_")
    );
    env::var_os(key).map(|version| version.to_string_lossy().into_owned())
}

/// Combines inventory and installed binary versions with the release check.
pub fn observed_upgrade_targets(
    metadata: &UpdateMetadata,
    discovered: &[ManagedUpgradeTarget],
    roster: Result<BTreeSet<String>, String>,
    index: Option<&ReleaseIndex>,
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
    observed.installed_core_version = discovered
        .iter()
        .find(|item| item.target.is_daemon())
        .map(|item| item.installed_version.clone());
    if let Some(index) = index {
        observed.available_core_version = index
            .components
            .get(Component::Core.label())
            .and_then(|entry| entry.version.clone());
        observed.requires_core = Component::ALL
            .into_iter()
            .filter_map(|component| {
                index
                    .components
                    .get(component.label())
                    .and_then(|entry| entry.requires_core.clone())
                    .map(|floor| (component, floor))
            })
            .collect();
    }
    match roster {
        Ok(modules) => {
            observed.supervised_modules = modules;
            observed.daemon_unreachable_reason = None;
        }
        Err(reason) => {
            observed.supervised_modules = BTreeSet::new();
            observed.daemon_unreachable_reason = Some(reason);
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
    /// Per target, the identity of the verified file just placed at its
    /// destination: the inode on Unix, the file's SHA-256 on Windows (see
    /// `destination_inode`). It is captured right after placement, before
    /// activation, so post-activation verification can check that the
    /// destination still holds the file this upgrade verified and placed, not a
    /// file something else put there in the meantime.
    activated_inodes: BTreeMap<String, String>,
    rollback_paths: BTreeMap<String, PathBuf>,
    rollback_archive_sha256: BTreeMap<String, Option<String>>,
    /// Per target, where Windows replacement moved the previous image while
    /// its process kept running. The image is deleted once the upgrade of
    /// that target is verified.
    replaced_images: BTreeMap<String, PathBuf>,
    expected_versions: BTreeMap<String, String>,
    /// The planner's `from` per target, empty for a binary that prints its own
    /// crate version (see `version_transition`). The completion line renders
    /// this rather than the binary's self-report so the plan and the result
    /// spell the same transition.
    planned_from: BTreeMap<String, String>,
    supervised_modules: BTreeSet<String>,
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
        let data_dir = load_current_inventory_root()?;
        let mut inventory = load_current_inventory()?;
        tighten_upgrade_permissions(&data_dir, &inventory)?;
        managed_replace::sweep_replaced_images(&mut inventory);
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
            activated_inodes: BTreeMap::new(),
            rollback_paths: BTreeMap::new(),
            rollback_archive_sha256: BTreeMap::new(),
            replaced_images: BTreeMap::new(),
            expected_versions,
            planned_from: BTreeMap::new(),
            supervised_modules: BTreeSet::new(),
        })
    }

    pub fn set_supervised_modules(&mut self, modules: BTreeSet<String>) {
        self.supervised_modules = modules;
    }

    pub fn is_module_supervised(&self, target: UpgradeTarget) -> bool {
        target
            .module_id()
            .is_some_and(|id| self.supervised_modules.contains(id))
    }

    pub fn set_expected_version(&mut self, target: UpgradeTarget, version: String) {
        self.expected_versions
            .insert(target.label().to_string(), version);
    }

    pub fn set_planned_from(&mut self, target: UpgradeTarget, from: String) {
        self.planned_from.insert(target.label().to_string(), from);
    }

    fn completion_line(&self, target: UpgradeTarget) -> String {
        let from = self
            .planned_from
            .get(target.label())
            .cloned()
            .unwrap_or_else(|| {
                self.targets
                    .get(target.label())
                    .expect("a completed upgrade has a discovered target")
                    .installed_version
                    .clone()
            });
        let to = self
            .expected_versions
            .get(target.label())
            .expect("a completed upgrade has an expected version");
        upgraded_line(target, &from, to)
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
        let module_id = target
            .module_id()
            .ok_or_else(|| format!("{target} is not a supervised module"))?;
        // A module restart rides on the daemon; if the daemon itself is
        // between incarnations when this polls, the status call fails and
        // that is "not yet" under the completion budget, not a verdict.
        let Ok(output) = self.run_ck(&["--json", "module", "status", module_id]) else {
            return Ok(false);
        };
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
        let target = self
            .targets
            .values()
            .find(|item| item.target.is_daemon())
            .ok_or_else(|| {
                "no managed daemon target was supplied for service verification".to_string()
            })?;
        let disk = self.daemon_build(target.target)?;
        let Some(subc) = &self.subc else {
            return Err(
                "no daemon connection file was supplied for service verification".to_string(),
            );
        };
        // Polled from the moment the service manager returns, which on macOS
        // is before the new daemon has bound its port or rewritten the
        // connection file. An unreadable file, a stale pid, or a refused
        // connection are "not yet" here. The completion loop separately checks
        // for an ended service with no daemon, so a failed start need not spend
        // the entire health budget.
        let Ok(connection) = connection_file::read_for_client(subc) else {
            return Ok(false);
        };
        if connection.pid == 0 {
            return Ok(false);
        }
        let Ok(response) = self.run_ck(&["--json", "daemon"]) else {
            return Ok(false);
        };
        let Ok(running) = serde_json::from_str::<Value>(&response) else {
            return Ok(false);
        };
        Ok(disk.matches_running(&running))
    }

    fn daemon_build(&self, target: UpgradeTarget) -> Result<CoreBuildInfo, String> {
        let destination = &self.target(target)?.destination;
        let output = super::components::run_probe_tolerating_text_busy(destination, "--build-info")
            .map_err(|error| format!("could not read managed ck-subc build identity: {error}"))?;
        if !output.status.success() {
            return Err("refusal: managed ck-subc does not report offline build identity; cannot verify the running core release".to_string());
        }
        let disk: CoreBuildInfo = serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("invalid managed ck-subc build identity: {error}"))?;
        if !disk.has_identity() {
            return Err("refusal: managed ck-subc build identity is unavailable; cannot verify the running core release".to_string());
        }
        let version = super::components::observed_binary_version(destination)
            .ok_or_else(|| "could not read managed ck-subc release version".to_string())?;
        if disk.core_version != version || version != self.expected_version(target)? {
            return Err(format!("refusal: managed ck-subc reports core {}, expected {}; cannot verify the running core release", disk.core_version, self.expected_version(target)?));
        }
        Ok(disk)
    }

    fn module_provenance(&self, target: UpgradeTarget) -> Result<(Option<u32>, bool), String> {
        let module_id = target
            .module_id()
            .ok_or_else(|| format!("{target} is not a supervised module"))?;
        let output = self.run_ck(&["--json", "provenance", module_id])?;
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

    fn place_candidate(
        &mut self,
        target: UpgradeTarget,
        destination: &Path,
        prepared: &PreparedUpgradeAsset,
    ) -> Result<String, String> {
        if target.is_self_replacing() {
            let version = self.expected_version(target)?.to_string();
            let evidence = self_update::replace_verified_candidate(
                destination,
                &prepared.candidate,
                &prepared.archive_sha256,
                &version,
                &mut self.inventory,
            )?;
            self.activated_inodes
                .insert(target.label().into(), destination_inode(destination)?);
            return Ok(evidence.to_string());
        }

        let parent = destination.parent().ok_or_else(|| {
            format!(
                "managed destination {} has no parent",
                destination.display()
            )
        })?;
        file_modes::ensure_owned_directory(parent, 0o755)?;
        let temporary = parent.join(format!(".{}.upgrade", target.label()));
        file_modes::copy_file_with_mode(&prepared.candidate, &temporary, 0o755).map_err(
            |error| {
                format!(
                    "could not place verified candidate at {}: {error}",
                    temporary.display()
                )
            },
        )?;
        let aside = match managed_replace::place_staged_binary(
            &temporary,
            destination,
            &mut self.inventory,
        ) {
            Ok(aside) => aside,
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                return Err(error);
            }
        };
        let aside_note = aside
            .as_ref()
            .map(|aside| format!("; previous image moved aside to {}", aside.display()))
            .unwrap_or_default();
        if let Some(aside) = aside {
            self.replaced_images.insert(target.label().into(), aside);
        }
        self.activated_inodes
            .insert(target.label().into(), destination_inode(destination)?);
        self.record_replacement_digest(target, destination, Some(&prepared.archive_sha256))?;
        Ok(format!(
            "destination replaced; inode={}{aside_note}",
            destination_inode(destination)?
        ))
    }

    fn discard_rollback_copy(&mut self, target: UpgradeTarget) {
        if let Some(rollback) = self.rollback_paths.remove(target.label()) {
            self.rollback_archive_sha256.remove(target.label());
            if let Err(error) = fs::remove_file(&rollback) {
                eprintln!(
                    "warning: could not remove unused rollback copy {}: {error}",
                    rollback.display()
                );
            }
        }
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
        file_modes::copy_file_with_mode(&destination, &rollback, 0o755).map_err(|error| {
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
        let prior_identity = destination_inode(&destination).ok();
        let result = self.place_candidate(target, &destination, &prepared);
        prepared.cleanup();
        if result.is_err()
            && prior_identity.is_some()
            && destination_inode(&destination).ok() == prior_identity
        {
            // The destination still holds the binary it held before, so the
            // rollback copy this run made of it is not needed. Removing it
            // leaves the bin directory exactly as this run found it.
            self.discard_rollback_copy(target);
        }
        result
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
        let module_id = target
            .module_id()
            .ok_or_else(|| format!("{target} is not a supervised module"))?;
        let output = self.run_ck(&["module", "restart", module_id, "--drain-ms", &drain])?;
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
        completion_timeout: Duration,
    ) -> Result<String, String> {
        let started = Instant::now();
        self.wait_until(completion_timeout, || self.module_ready(target))?;
        Ok(format!(
            "module is live and healthy {:.0}s after restart (budget {}s)",
            started.elapsed().as_secs_f64(),
            completion_timeout.as_secs()
        ))
    }

    fn restart_daemon_via_service_manager(
        &mut self,
        drain_timeout: Duration,
    ) -> Result<String, String> {
        let platform = super::runtime::RuntimePlatform::current();
        let root = load_current_inventory_root()?;
        let paths =
            super::runtime::runtime_paths(platform, &root.join("bin"), &super::apply::user_home()?);
        let detail = super::runtime::restart_with_definition_at(
            platform,
            &paths,
            &mut super::runtime::SystemCommandRunner,
            &mut self.inventory,
            self.subc.as_deref(),
        )?;
        Ok(format!(
            "{detail}; drain budget={}s",
            drain_timeout.as_secs()
        ))
    }

    fn poll_daemon_service_ready(
        &mut self,
        completion_timeout: Duration,
    ) -> Result<String, String> {
        let started = Instant::now();
        let platform = super::runtime::RuntimePlatform::current();
        let root = load_current_inventory_root()?;
        let paths =
            super::runtime::runtime_paths(platform, &root.join("bin"), &super::apply::user_home()?);
        super::runtime::wait_for_service_ready(
            platform,
            &paths,
            &mut super::runtime::SystemCommandRunner,
            completion_timeout,
            || self.daemon_ready(),
        )?;
        Ok(format!(
            "daemon service is live and healthy {:.0}s after restart (budget {}s)",
            started.elapsed().as_secs_f64(),
            completion_timeout.as_secs()
        ))
    }

    fn post_verify(&mut self, target: UpgradeTarget) -> Result<String, String> {
        let destination = &self.target(target)?.destination;
        let is_supervised_module =
            target.module_id().is_some() && self.is_module_supervised(target);
        let (pid, healthy, running_image_matches_destination, version) = if is_supervised_module {
            let (pid, running_image_matches_destination) = self.module_provenance(target)?;
            let healthy = self.module_ready(target)?;
            (
                pid,
                healthy,
                running_image_matches_destination,
                binary_version(destination)?,
            )
        } else if target.is_daemon() {
            let disk = self.daemon_build(target)?;
            let subc = self.subc.as_ref().ok_or_else(|| {
                "no daemon connection file was supplied for verification".to_string()
            })?;
            let info = connection_file::read_for_client(subc)
                .map_err(|error| format!("could not read daemon connection: {error}"))?;
            if info.pid == 0 {
                return Err("daemon connection did not report a live pid".to_string());
            }
            let response = self.run_ck(&["--json", "daemon"])?;
            let running: Value = serde_json::from_str(&response)
                .map_err(|error| format!("invalid daemon build response: {error}"))?;
            // A healthy old daemon can have the same library version. Only its
            // own build identity can tie the running process to the placed core.
            (
                Some(info.pid),
                true,
                disk.matches_running(&running),
                disk.core_version,
            )
        } else if target.is_self_replacing() {
            (Some(process_id()), true, true, binary_version(destination)?)
        } else {
            (None, false, false, binary_version(destination)?)
        };
        // Keep the existing reported-version exemption limited to the two
        // legacy targets whose releases were already accepted this way.
        let expected_version = if target.accepts_reported_version()
            || self.assets.accepts_reported_version(target, self.platform)
        {
            version.clone()
        } else {
            self.expected_version(target)?.to_string()
        };
        let require_live_process = is_supervised_module || target.is_daemon();
        let expectation = expected_post_activation(
            self.activated_inodes
                .get(target.label())
                .ok_or_else(|| format!("no replacement identity recorded for {target}"))?
                .clone(),
            expected_version,
            require_live_process,
            require_live_process,
        );
        let evidence = VerificationEvidence {
            pid,
            inode: destination_inode(destination)?,
            healthy,
            version,
            running_image_matches_destination,
        };
        let detail = verify_post_activation(&evidence, &expectation).map_err(|error| {
            format!(
                "{}: {error}",
                super::upgrade_verification::target_verification_label(target)
            )
        })?;
        if target.module_id().is_some() && !is_supervised_module {
            Ok(format!(
                "{detail}; module is not supervised on this host and was verified by binary version only"
            ))
        } else {
            Ok(detail)
        }
    }

    fn completed(&mut self, target: UpgradeTarget) {
        // Failed targets keep their recovery evidence; a verified completed
        // replacement no longer needs a private copy of the previous image.
        if let Some(path) = self.rollback_paths.get(target.label()) {
            match fs::remove_file(path) {
                Ok(()) => {
                    self.rollback_paths.remove(target.label());
                    self.rollback_archive_sha256.remove(target.label());
                }
                Err(error) => eprintln!(
                    "warning: upgrade completed but could not remove rollback copy {}: {error}",
                    path.display()
                ),
            }
        }
        // By now the restarted process runs the new image, so the previous
        // one is normally free to delete. If its process is still exiting,
        // the inventory row stays and the next ck setup or ck upgrade
        // deletes it.
        if let Some(aside) = self.replaced_images.remove(target.label()) {
            if let Err(error) = managed_replace::remove_replaced_image(&mut self.inventory, &aside)
            {
                eprintln!(
                    "warning: upgrade completed but {error}; a later ck setup or ck upgrade removes it"
                );
            }
        }
        println!("{}", self.completion_line(target));
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
            .ok_or_else(|| format!("no rollback copy exists for {target}"))?
            .clone();
        #[cfg(unix)]
        let moved_aside = if target.is_self_replacing() {
            super::self_update_unix::replace_verified_candidate(&destination, &rollback)?;
            file_modes::set_new_file_mode(&destination, 0o755)?;
            None
        } else {
            restore_rollback_by_rename(&rollback, &destination, &mut self.inventory, false)?
        };
        // Windows refuses to rename over the image of a running process, and
        // a rollback usually follows a new image that did start, so the new
        // image is moved aside first. Its process keeps running from there;
        // the next ck setup or ck upgrade deletes the file once it is free.
        #[cfg(not(unix))]
        let moved_aside = restore_rollback_by_rename(
            &rollback,
            &destination,
            &mut self.inventory,
            cfg!(windows),
        )?;
        let previous_archive = self
            .rollback_archive_sha256
            .remove(target.label())
            .flatten();
        self.record_replacement_digest(target, &destination, previous_archive.as_deref())?;
        fs::remove_file(&rollback).map_err(|error| {
            format!(
                "restored {} but could not remove rollback copy {}: {error}",
                destination.display(),
                rollback.display()
            )
        })?;
        self.rollback_paths.remove(target.label());
        let aside_note = moved_aside
            .map(|aside| {
                format!(
                    "; the replaced image was moved aside to {} until its process exits",
                    aside.display()
                )
            })
            .unwrap_or_default();
        Ok(format!(
            "accepted; restored prior binary at inode={}{aside_note}",
            destination_inode(&destination)?
        ))
    }
}

/// Copies the rollback copy beside `destination` and renames it into place,
/// through the same step as a forward replacement: with `move_previous_aside`
/// (Windows) the image at the destination is first moved aside and recorded
/// for a later sweep, and the aside path is returned. The rollback copy itself
/// is left for the caller to remove.
fn restore_rollback_by_rename(
    rollback: &Path,
    destination: &Path,
    inventory: &mut Inventory,
    move_previous_aside: bool,
) -> Result<Option<PathBuf>, String> {
    let parent = destination
        .parent()
        .ok_or_else(|| format!("{} has no parent", destination.display()))?;
    let temporary = parent.join(format!(
        ".{}.restore-{}-{}",
        destination
            .file_name()
            .unwrap_or_default()
            .to_string_lossy(),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos()
    ));
    let result = (|| {
        #[cfg(unix)]
        file_modes::copy_file_with_mode(rollback, &temporary, 0o755)
            .map_err(|error| format!("could not stage rollback {}: {error}", rollback.display()))?;
        #[cfg(not(unix))]
        {
            fs::copy(rollback, &temporary).map_err(|error| {
                format!("could not stage rollback {}: {error}", rollback.display())
            })?;
            let permissions = fs::metadata(rollback)
                .map_err(|error| format!("could not read rollback mode: {error}"))?
                .permissions();
            fs::set_permissions(&temporary, permissions)
                .map_err(|error| format!("could not restore rollback mode: {error}"))?;
        }
        managed_replace::place_with(&temporary, destination, inventory, move_previous_aside)
            .map_err(|error| format!("could not put the rollback copy in place: {error}"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn render_execution_report(report: &UpgradeExecutionReport) {
    for evidence in &report.evidence {
        println!("{evidence}");
    }
}

pub fn upgraded_line(target: UpgradeTarget, from: &str, to: &str) -> String {
    let restarted = if target.is_daemon() || target.module_id().is_some() {
        ", restarted"
    } else {
        ""
    };
    format!(
        "upgraded {}{restarted}",
        super::model::version_transition(&target.to_string(), from, to)
    )
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
            super::model::CoreVersion::from_release(token).is_ok()
                // MC's owner publishes build trains rather than numeric
                // releases, and its --version line prints that train tag.
                || token.strip_prefix("ck-mc-").is_some_and(|train| !train.is_empty()
                    && train.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')))
        })
        .map(ToOwned::to_owned)
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
    use serde_json::Map;
    #[cfg(unix)]
    use serde_json::Value;

    use super::*;
    #[cfg(unix)]
    use crate::setup::test_exec::{copy_executable, write_executable};
    use crate::setup::{components::ReleaseArtifactSource, planner::plan_upgrade};
    use cortexkit_test_support::ScratchDir;

    fn upgrade_target(binary: &str) -> UpgradeTarget {
        upgrade_roster(Component::ALL)
            .into_iter()
            .find(|target| target.label() == binary)
            .unwrap_or_else(|| panic!("missing upgrade target {binary}"))
    }

    // Used only by the unix-gated tests below; gate it with them so windows
    // clippy under -D warnings does not read it as dead code.
    #[cfg(unix)]
    fn fixture_dir(name: &str) -> ScratchDir {
        ScratchDir::new(name)
    }

    #[cfg(unix)]
    fn version_binary(path: &Path, version: &str) {
        write_executable(
            path,
            format!("#!/bin/sh\necho 'binary {version}'\n").as_bytes(),
        );
    }

    #[cfg(unix)]
    #[test]
    fn upgrade_tightens_existing_modes_only_on_owned_files() {
        use std::os::unix::fs::PermissionsExt;

        let root = fixture_dir("upgrade-mode-repair");
        let data_dir = root.join("cortexkit");
        let binary_dir = data_dir.join("bin");
        let config_dir = root.join(".config/cortexkit");
        let runtime_dir = root.join("Library/LaunchAgents");
        let claustrum_dir = data_dir.join("claustrum");
        let directories = [
            data_dir.clone(),
            binary_dir.clone(),
            config_dir.clone(),
            runtime_dir.clone(),
            claustrum_dir.clone(),
            data_dir.join("cmd"),
            data_dir.join("stores"),
        ];
        for directory in &directories {
            fs::create_dir_all(directory).unwrap();
        }
        let binary = binary_dir.join("ck-aft");
        let config = config_dir.join("subc.jsonc");
        let runtime = runtime_dir.join("cortexkit.subc.plist");
        let key = claustrum_dir.join("master.key");
        let manifest = data_dir.join("installer-manifest.json");
        let unrelated = data_dir.join("user-note.txt");
        for path in [&binary, &config, &runtime, &key, &unrelated] {
            fs::write(path, "contents").unwrap();
        }
        let mut inventory = Inventory::load(&manifest, "macos-arm64").unwrap();
        inventory.record("managed-binary", &binary, Map::new());
        inventory.record("configuration", &config, Map::new());
        inventory.record("runtime-definition", &runtime, Map::new());
        inventory.save().unwrap();

        for directory in &directories {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o777)).unwrap();
        }
        for path in [&binary, &config, &runtime, &key, &manifest, &unrelated] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o666)).unwrap();
        }
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o777)).unwrap();

        tighten_upgrade_permissions(&data_dir, &inventory).unwrap();

        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&data_dir), 0o700);
        assert_eq!(mode(&binary_dir), 0o755);
        assert_eq!(mode(&data_dir.join("cmd")), 0o755);
        assert_eq!(mode(&config_dir), 0o700);
        assert_eq!(mode(&runtime_dir), 0o755);
        assert_eq!(mode(&claustrum_dir), 0o700);
        assert_eq!(mode(&binary), 0o755);
        assert_eq!(mode(&config), 0o600);
        assert_eq!(mode(&runtime), 0o644);
        assert_eq!(mode(&key), 0o600);
        assert_eq!(mode(&manifest), 0o644);
        assert_eq!(mode(&unrelated), 0o666);
    }

    #[cfg(unix)]
    fn isolated_backend(root: &Path, target: UpgradeTarget) -> SystemUpgradeBackend {
        SystemUpgradeBackend {
            platform: AlphaTarget::LinuxX64,
            targets: [(
                target.label().to_string(),
                ManagedUpgradeTarget {
                    target,
                    destination: root.join(target.label()),
                    installed_version: "0.1.0".into(),
                    installed_archive_sha256: None,
                },
            )]
            .into_iter()
            .collect(),
            executable: root.join("ck"),
            subc: None,
            assets: ReleaseUpgradeAssetFetcher::from_index(ReleaseIndex {
                schema: 1,
                channel: "alpha".into(),
                generated_at_ms: 0,
                components: BTreeMap::new(),
            }),
            inventory: Inventory::load(root.join("installer-manifest.json"), "linux-x64").unwrap(),
            prepared: BTreeMap::new(),
            activated_inodes: [(
                target.label().into(),
                destination_inode(&root.join(target.label())).unwrap(),
            )]
            .into_iter()
            .collect(),
            rollback_paths: BTreeMap::new(),
            rollback_archive_sha256: BTreeMap::new(),
            replaced_images: BTreeMap::new(),
            expected_versions: [(target.label().to_string(), "0.1.0".into())]
                .into_iter()
                .collect(),
            planned_from: BTreeMap::new(),
            supervised_modules: BTreeSet::new(),
        }
    }

    #[cfg(unix)]
    fn daemon_build_backend(root: &Path, disk: Value, running: Value) -> SystemUpgradeBackend {
        write_executable(&root.join("ck-subc"), format!(
            "#!/bin/sh\nif [ \"$1\" = --build-info ]; then\n  echo '{disk}'\nelse\n  echo 'ck-subc 0.20.83'\nfi\n"
        ).as_bytes());
        write_executable(
            &root.join("ck"),
            format!("#!/bin/sh\necho '{running}'\n").as_bytes(),
        );
        let target = upgrade_target("ck-subc");
        let mut backend = isolated_backend(root, target);
        backend.set_expected_version(target, "0.20.83".to_string());
        let connection_path = root.join("connection.json");
        subc_transport::write_atomic(
            &connection_path,
            &subc_transport::ConnectionInfo {
                schema: subc_transport::SCHEMA_VERSION,
                wire_version: Some(subc_protocol::PROTOCOL_VERSION),
                endpoints: vec![subc_transport::Endpoint {
                    host: "127.0.0.1".to_string(),
                    port: 1,
                }],
                key: vec![7; 32],
                daemon_id: [8; 16],
                pid: 44,
                daemon_ver: "0.36.5".to_string(),
            },
        )
        .unwrap();
        backend.subc = Some(connection_path);
        backend
    }

    #[cfg(unix)]
    fn core_build(sha: Option<&str>, lock: Option<&str>) -> Value {
        serde_json::json!({"core_version": "0.20.83", "build_git_sha": sha, "build_lock_digest": lock})
    }

    #[cfg(unix)]
    #[test]
    fn daemon_readiness_accepts_matching_core_build_with_library_0_36_5() {
        let root = fixture_dir("daemon-ready-core-build");
        let build = core_build(Some(&"a".repeat(40)), Some(&"1".repeat(64)));
        let backend = daemon_build_backend(root.path(), build.clone(), build);
        assert!(backend.daemon_ready().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn daemon_readiness_waits_for_old_build_with_same_library_version() {
        let root = fixture_dir("daemon-ready-old-build");
        let disk = core_build(Some(&"a".repeat(40)), Some(&"1".repeat(64)));
        let old = core_build(Some(&"b".repeat(40)), Some(&"1".repeat(64)));
        let backend = daemon_build_backend(root.path(), disk, old);
        assert!(!backend.daemon_ready().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn daemon_post_verify_accepts_matching_core_build_with_library_0_36_5() {
        let root = fixture_dir("daemon-post-core-build");
        let build = core_build(Some(&"a".repeat(40)), Some(&"1".repeat(64)));
        let mut backend = daemon_build_backend(root.path(), build.clone(), build);
        let evidence = backend.post_verify(upgrade_target("ck-subc")).unwrap();
        assert!(evidence.contains("version=0.20.83"), "{evidence}");
        assert!(evidence.contains("running-image=matched"), "{evidence}");
    }

    #[cfg(unix)]
    #[test]
    fn daemon_post_verify_rejects_old_build_with_same_library_version() {
        let root = fixture_dir("daemon-post-old-build");
        let disk = core_build(Some(&"a".repeat(40)), Some(&"1".repeat(64)));
        let old = core_build(Some(&"b".repeat(40)), Some(&"1".repeat(64)));
        let mut backend = daemon_build_backend(root.path(), disk, old);
        let error = backend.post_verify(upgrade_target("ck-subc")).unwrap_err();
        assert!(error.contains("running"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn daemon_activation_never_accepts_absent_or_dirty_build_identity() {
        let root = fixture_dir("daemon-unverifiable-build");
        let sha = "a".repeat(40);
        let lock = "1".repeat(64);
        for disk in [
            core_build(None, Some(&lock)),
            core_build(Some("unavailable"), Some(&lock)),
            core_build(Some(&format!("{sha}-dirty")), Some(&lock)),
            core_build(Some(&sha), None),
        ] {
            let mut backend = daemon_build_backend(root.path(), disk.clone(), disk);
            assert!(backend
                .daemon_ready()
                .unwrap_err()
                .contains("build identity is unavailable"));
            assert!(backend
                .post_verify(upgrade_target("ck-subc"))
                .unwrap_err()
                .contains("build identity is unavailable"));
        }
        for running in [
            core_build(None, Some(&lock)),
            core_build(Some(&sha), None),
            core_build(Some(&sha), Some(&"2".repeat(64))),
        ] {
            let mut backend =
                daemon_build_backend(root.path(), core_build(Some(&sha), Some(&lock)), running);
            assert!(!backend.daemon_ready().unwrap());
            assert!(backend.post_verify(upgrade_target("ck-subc")).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn train_version_output_is_discovered_and_post_verified() {
        let root = ScratchDir::new("train-version-output");
        let target = upgrade_target("ck-mc");
        let version = "ck-mc-alpha.22464bf2";
        version_binary(&root.join("ck-mc"), version);
        assert_eq!(binary_version(&root.join("ck-mc")).unwrap(), version);
        let mut backend = isolated_backend(&root, target);
        backend.set_expected_version(target, version.into());
        assert!(backend.post_verify(target).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn successful_upgrade_completion_removes_its_rollback_copy() {
        let root = ScratchDir::new("upgrade-completed-rollback");
        let target = upgrade_target("ck-mc");
        version_binary(&root.join("ck-mc"), "0.1.0");
        let mut backend = isolated_backend(&root, target);
        backend.create_rollback_copy(target).unwrap();
        let path = backend.rollback_paths.get(target.label()).unwrap().clone();
        assert!(path.is_file());
        backend.completed(target);
        assert!(!path.exists(), "{}", path.display());
        assert!(!backend.rollback_paths.contains_key(target.label()));
    }

    #[cfg(unix)]
    #[test]
    fn post_verification_rejects_a_destination_changed_after_replacement() {
        let root = ScratchDir::new("changed-post-inode");
        let target = upgrade_target("ck-mc");
        let destination = root.join("ck-mc");
        version_binary(&destination, "0.1.0");
        let mut backend = isolated_backend(&root, target);
        let workspace = root.join("candidate");
        backend
            .inventory
            .record("managed-binary", &destination, serde_json::Map::new());
        fs::create_dir(&workspace).unwrap();
        let candidate = workspace.join("ck-mc");
        version_binary(&candidate, "0.1.0");
        backend.prepared.insert(
            target.label().into(),
            PreparedUpgradeAsset::test_candidate(candidate, target),
        );
        backend.replace_destination(target).unwrap();
        assert!(backend.post_verify(target).is_ok());
        let rogue = root.join("rogue");
        version_binary(&rogue, "0.1.0");
        fs::rename(rogue, &destination).unwrap();
        let error = backend
            .post_verify(target)
            .expect_err("changed destination must fail");
        assert!(error.contains("destination inode mismatch"), "{error}");
    }

    /// A backend that owns one managed binary at `destination`, with the
    /// inventory kept outside the binary's directory so tests can compare that
    /// directory's whole listing. Builds on every platform: the destination
    /// need not be executable until something runs it.
    fn managed_backend(
        root: &Path,
        target: UpgradeTarget,
        destination: &Path,
    ) -> SystemUpgradeBackend {
        let mut inventory =
            Inventory::load(root.join("installer-manifest.json"), "linux-x64").unwrap();
        inventory.record("managed-binary", destination, Map::new());
        inventory.save().unwrap();
        SystemUpgradeBackend {
            platform: AlphaTarget::LinuxX64,
            targets: BTreeMap::from([(
                target.label().to_string(),
                ManagedUpgradeTarget {
                    target,
                    destination: destination.to_path_buf(),
                    installed_version: "0.1.0".into(),
                    installed_archive_sha256: None,
                },
            )]),
            executable: root.join("ck"),
            subc: None,
            assets: ReleaseUpgradeAssetFetcher::from_index(ReleaseIndex {
                schema: 1,
                channel: "alpha".into(),
                generated_at_ms: 0,
                components: BTreeMap::new(),
            }),
            inventory,
            prepared: BTreeMap::new(),
            activated_inodes: BTreeMap::new(),
            rollback_paths: BTreeMap::new(),
            rollback_archive_sha256: BTreeMap::new(),
            replaced_images: BTreeMap::new(),
            expected_versions: BTreeMap::from([(target.label().to_string(), "0.2.0".into())]),
            planned_from: BTreeMap::new(),
            supervised_modules: BTreeSet::new(),
        }
    }

    /// Hands the backend a verified candidate with `bytes`, in its own
    /// workspace outside the binary's directory, as `download_and_verify` would.
    fn stage_candidate(
        backend: &mut SystemUpgradeBackend,
        root: &Path,
        target: UpgradeTarget,
        bytes: &[u8],
    ) {
        let workspace = root.join("candidate-workspace");
        fs::create_dir(&workspace).unwrap();
        let candidate = workspace.join("candidate");
        fs::write(&candidate, bytes).unwrap();
        backend.prepared.insert(
            target.label().into(),
            PreparedUpgradeAsset::test_candidate(candidate, target),
        );
    }

    fn directory_entries(directory: &Path) -> Vec<String> {
        let mut names = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn a_replacement_refused_before_the_destination_changed_leaves_the_bin_directory_as_found() {
        let root = ScratchDir::new("upgrade-refused-replacement");
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        let target = upgrade_target("ck-aft");
        let destination = bin.join("ckdev-aft.exe");
        fs::write(&destination, "original image").unwrap();
        let mut backend = managed_backend(&root, target, &destination);
        stage_candidate(&mut backend, &root, target, b"verified candidate");
        let found = directory_entries(&bin);

        backend.create_rollback_copy(target).unwrap();
        let rollback = backend.rollback_paths[target.label()].clone();
        assert!(rollback.is_file(), "{}", rollback.display());
        // The staged candidate is the file whose rename is refused, the way
        // Windows refuses to replace the image of a running process.
        let _refusal = managed_replace::test_seam::refuse_rename_from(&bin.join(".ck-aft.upgrade"));
        let error = backend
            .replace_destination(target)
            .expect_err("the refused rename fails the replacement");

        assert!(error.contains("rename refused by test"), "{error}");
        assert_eq!(fs::read(&destination).unwrap(), b"original image");
        assert_eq!(
            directory_entries(&bin),
            found,
            "no staging file, rollback copy or moved-aside image may remain"
        );
        assert!(!backend.rollback_paths.contains_key(target.label()));
        assert!(backend
            .inventory
            .paths_for_kind(managed_replace::REPLACED_IMAGE_KIND)
            .is_empty());
    }

    /// Starts a copy of this test binary at `destination` that keeps running,
    /// standing in for a daemon or module the upgrade has not restarted yet.
    #[cfg(windows)]
    fn start_running_image(destination: &Path) -> std::process::Child {
        fs::copy(std::env::current_exe().unwrap(), destination).unwrap();
        let mut child = cortexkit_test_support::dev_command(destination)
            .args([
                "--exact",
                "setup::upgrade::tests::rollback_stand_in_holds_until_killed",
                "--test-threads=1",
            ])
            .env(ROLLBACK_HOLD_ENV, "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // A stand-in that exited at once would prove nothing about replacing a
        // running image, so fail loudly instead.
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            child.try_wait().unwrap().is_none(),
            "the stand-in for the running managed binary exited before the replacement"
        );
        child
    }

    #[cfg(windows)]
    #[test]
    fn windows_replacement_moves_a_running_image_aside() {
        let root = ScratchDir::new("upgrade-running-image");
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        let target = upgrade_target("ck-aft");
        let destination = bin.join("ckdev-aft.exe");
        let mut child = start_running_image(&destination);
        let mut backend = managed_backend(&root, target, &destination);
        stage_candidate(&mut backend, &root, target, b"verified candidate");
        backend.create_rollback_copy(target).unwrap();

        let result = backend.replace_destination(target);
        let still_running = child.try_wait().unwrap().is_none();
        let destination_bytes = fs::read(&destination);
        let staging_left = bin.join(".ck-aft.upgrade").exists();
        child.kill().unwrap();
        child.wait().unwrap();

        result.expect("replacing a running image succeeds");
        assert!(
            still_running,
            "the old process keeps running from the aside file"
        );
        assert_eq!(destination_bytes.unwrap(), b"verified candidate");
        assert!(
            !staging_left,
            "the staging file was renamed onto the destination"
        );
        let aside = backend.replaced_images[target.label()].clone();
        assert!(aside.is_file(), "{}", aside.display());

        // With the old process gone, completion deletes the aside image and
        // the rollback copy. Windows can release an exited image a moment
        // late; the sweep a later ck setup or ck upgrade runs covers that.
        backend.completed(target);
        let deadline = Instant::now() + Duration::from_secs(30);
        while aside.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            managed_replace::sweep_replaced_images(&mut backend.inventory);
        }
        assert_eq!(directory_entries(&bin), ["ckdev-aft.exe"]);
        assert!(backend
            .inventory
            .paths_for_kind(managed_replace::REPLACED_IMAGE_KIND)
            .is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_refused_replacement_moves_the_running_image_back() {
        let root = ScratchDir::new("upgrade-running-image-restored");
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        let target = upgrade_target("ck-aft");
        let destination = bin.join("ckdev-aft.exe");
        let mut child = start_running_image(&destination);
        let original = fs::read(&destination).unwrap();
        let mut backend = managed_backend(&root, target, &destination);
        stage_candidate(&mut backend, &root, target, b"verified candidate");
        backend.create_rollback_copy(target).unwrap();

        let _refusal = managed_replace::test_seam::refuse_rename_from(&bin.join(".ck-aft.upgrade"));
        let result = backend.replace_destination(target);
        let still_running = child.try_wait().unwrap().is_none();
        let destination_bytes = fs::read(&destination);
        let entries = directory_entries(&bin);
        child.kill().unwrap();
        child.wait().unwrap();

        let error = result.expect_err("the refused rename fails the replacement");
        assert!(error.contains("rename refused by test"), "{error}");
        assert!(!error.contains("additionally"), "{error}");
        assert!(still_running, "the running process is left alone");
        assert_eq!(destination_bytes.unwrap(), original);
        assert_eq!(
            entries,
            ["ckdev-aft.exe"],
            "the image is back under its name and nothing else is left"
        );
        assert!(backend
            .inventory
            .paths_for_kind(managed_replace::REPLACED_IMAGE_KIND)
            .is_empty());
    }

    /// The rollback goes through the same aside step as the forward
    /// replacement. Forced on here so it runs on every platform; Windows takes
    /// this path for real.
    #[test]
    fn rollback_restore_moves_the_replaced_image_aside_and_records_it() {
        let root = ScratchDir::new("rollback-restore-aside");
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        let destination = bin.join("ckdev-aft.exe");
        let rollback = bin.join("ckdev-aft.exe.rollback");
        fs::write(&destination, "new image").unwrap();
        fs::write(&rollback, "previous image").unwrap();
        let mut inventory =
            Inventory::load(root.join("installer-manifest.json"), "windows-x64").unwrap();

        let aside = restore_rollback_by_rename(&rollback, &destination, &mut inventory, true)
            .unwrap()
            .expect("the replaced image is moved aside");

        assert_eq!(fs::read(&destination).unwrap(), b"previous image");
        assert_eq!(fs::read(&aside).unwrap(), b"new image");
        assert!(inventory.owns_path(managed_replace::REPLACED_IMAGE_KIND, &aside));
        let mut expected = vec![
            aside.file_name().unwrap().to_string_lossy().into_owned(),
            "ckdev-aft.exe".to_string(),
            "ckdev-aft.exe.rollback".to_string(),
        ];
        expected.sort();
        assert_eq!(
            directory_entries(&bin),
            expected,
            "the staged copy was renamed into place; the rollback copy is the caller's to remove"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_rollback_moves_the_running_new_image_aside() {
        let root = ScratchDir::new("rollback-running-image");
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        let target = upgrade_target("ck-aft");
        let destination = bin.join("ckdev-aft.exe");
        // The new image is running: the restarted module started but failed
        // verification, and the operator accepted the rollback.
        let mut child = start_running_image(&destination);
        let mut backend = managed_backend(&root, target, &destination);
        let rollback = bin.join("ckdev-aft.exe.rollback");
        fs::write(&rollback, "previous release").unwrap();
        backend
            .rollback_paths
            .insert(target.label().into(), rollback.clone());

        let result = backend.rollback(target);
        let still_running = child.try_wait().unwrap().is_none();
        let destination_bytes = fs::read(&destination);
        let recorded = backend
            .inventory
            .paths_for_kind(managed_replace::REPLACED_IMAGE_KIND);
        child.kill().unwrap();
        child.wait().unwrap();

        let detail = result.expect("rolling back over a running image succeeds");
        assert!(detail.contains("moved aside"), "{detail}");
        assert!(
            still_running,
            "the new process keeps running from the aside file"
        );
        assert_eq!(destination_bytes.unwrap(), b"previous release");
        assert!(!rollback.exists(), "a completed rollback removes its copy");
        let [aside] = recorded.as_slice() else {
            panic!("exactly one moved-aside image is recorded: {recorded:?}");
        };
        assert!(aside.is_file(), "{}", aside.display());

        // With its process gone, the sweep a later ck setup or ck upgrade
        // runs deletes the moved-aside image. Windows can release an exited
        // image a moment late, so the sweep is retried briefly.
        let deadline = Instant::now() + Duration::from_secs(30);
        while aside.exists() && Instant::now() < deadline {
            managed_replace::sweep_replaced_images(&mut backend.inventory);
            if aside.exists() {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        assert!(!aside.exists(), "{}", aside.display());
        assert_eq!(directory_entries(&bin), ["ckdev-aft.exe"]);
        assert!(backend
            .inventory
            .paths_for_kind(managed_replace::REPLACED_IMAGE_KIND)
            .is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn train_release_post_verifies_the_binarys_own_version() {
        let root = ScratchDir::new("train-post-verify");
        let target = upgrade_target("ck-mc");
        version_binary(&root.join("ck-mc"), "0.1.0");
        let mut backend = isolated_backend(&root, target);
        backend.assets = ReleaseUpgradeAssetFetcher::from_index(serde_json::from_value(serde_json::json!({
            "schema":1, "channel":"alpha", "generated_at_ms":0,
            "components":{"mc":{"release":"ck-mc-deadbeef", "version":null,
                "assets":{"linux-x64":{"ck-mc":{"url":"https://example.invalid/mc.zip", "sha256":"00", "reports":null}}}}}
        })).unwrap());
        backend.set_expected_version(target, "ck-mc-deadbeef".into());
        assert!(backend.post_verify(target).is_ok());
    }

    /// Environment variable that turns [`rollback_stand_in_holds_until_killed`]
    /// from an instant pass into a process that stays alive.
    const ROLLBACK_HOLD_ENV: &str = "CK_ROLLBACK_TEST_HOLD";

    /// Run normally, this passes at once. Run from a copy of this test binary
    /// with [`ROLLBACK_HOLD_ENV`] set, it keeps that copy running so the tests
    /// that replace or roll back a managed binary have a live executable at
    /// the destination.
    #[test]
    fn rollback_stand_in_holds_until_killed() {
        if std::env::var_os(ROLLBACK_HOLD_ENV).is_some() {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    }

    #[cfg(unix)]
    #[test]
    fn rollback_replaces_running_executable_by_rename() {
        let root = fixture_dir("rollback-running-executable");
        // The managed binary this test models runs as `ckdev-aft`, so the test
        // process is never listed under a production `ck-` name.
        let destination = root.join("ckdev-aft");
        let rollback = root.join("ckdev-aft.rollback");
        let candidate = root.join("candidate");
        // The stand-in is a copy of this test binary, not of a system utility:
        // macOS kills a copied Apple platform binary run from outside its system
        // path, and a multicall coreutils refuses to run under another name, so
        // either way the "running" executable would already be gone.
        let test_binary = std::env::current_exe().expect("test executable");
        copy_executable(&test_binary, &destination);
        copy_executable(&test_binary, &rollback);
        copy_executable(&test_binary, &candidate);
        fs::rename(&candidate, &destination).unwrap();
        let replaced_inode = destination_inode(&destination).unwrap();
        let mut child = Command::new(&destination)
            .args([
                "--exact",
                "setup::upgrade::tests::rollback_stand_in_holds_until_killed",
                "--test-threads=1",
            ])
            .env(ROLLBACK_HOLD_ENV, "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // A stand-in that exited at once would prove nothing about replacing a
        // running executable, so fail loudly instead.
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(
            child.try_wait().unwrap().is_none(),
            "the stand-in for the running managed binary exited before the rollback"
        );
        let aft = upgrade_target("ck-aft");
        let mut inventory =
            Inventory::load(root.join("installer-manifest.json"), "linux-x64").unwrap();
        inventory.record("managed-binary", &destination, Map::new());
        let mut backend = SystemUpgradeBackend {
            platform: AlphaTarget::LinuxX64,
            targets: BTreeMap::from([(
                aft.label().to_string(),
                ManagedUpgradeTarget {
                    target: aft,
                    destination: destination.clone(),
                    installed_version: "1.0.0".to_string(),
                    installed_archive_sha256: None,
                },
            )]),
            executable: destination.clone(),
            subc: None,
            assets: ReleaseUpgradeAssetFetcher::from_index(ReleaseIndex {
                schema: 1,
                channel: "alpha".to_string(),
                generated_at_ms: 0,
                components: BTreeMap::new(),
            }),
            inventory,
            prepared: BTreeMap::new(),
            activated_inodes: BTreeMap::new(),
            rollback_paths: BTreeMap::from([(aft.label().to_string(), rollback)]),
            rollback_archive_sha256: BTreeMap::new(),
            replaced_images: BTreeMap::new(),
            expected_versions: BTreeMap::new(),
            planned_from: BTreeMap::new(),
            supervised_modules: BTreeSet::new(),
        };
        let result = backend.rollback(aft);
        child.kill().unwrap();
        child.wait().unwrap();
        result.expect("rollback onto running executable");
        assert!(
            !root.join("ckdev-aft.rollback").exists(),
            "successful rollback cleans its backup"
        );
        assert_ne!(
            destination_inode(&destination).unwrap(),
            replaced_inode,
            "rollback must rename a new inode over the running executable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn discovery_uses_inventory_and_version_outputs_for_every_installed_component() {
        let root = fixture_dir("discovery");
        let ck = root.join("ck");
        let mcp = root.join("ck-subc-mcp");
        let aft = root.join("ck-aft");
        let daemon = root.join("ck-subc");
        let mc = root.join("ck-mc");
        version_binary(&ck, "1.0.0");
        version_binary(&mcp, "1.1.0");
        version_binary(&aft, "1.2.0");
        version_binary(&daemon, "1.3.0");
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
                version: "0.36.5".to_string(),
            }),
        )
        .expect("discover targets");
        assert_eq!(
            targets
                .iter()
                .map(|item| item.target.label())
                .collect::<Vec<_>>(),
            ["ck-subc", "ck-subc-mcp", "ck-aft", "ck-mc", "ck"]
        );
        // Every version names its installed binary, not the daemon library.
        assert_eq!(targets[0].installed_version, "1.3.0");
        assert_eq!(targets[1].installed_version, "1.1.0");
        assert_eq!(targets[2].installed_version, "1.2.0");
        assert_eq!(targets[3].installed_version, "99.0.0");
        assert_eq!(targets[4].installed_version, "1.0.0");
    }

    #[cfg(unix)]
    #[test]
    fn discovery_uses_binary_core_release_instead_of_library_version() {
        let root = fixture_dir("discovery-core-release");
        let daemon = root.join("ck-subc");
        let mcp = root.join("ck-subc-mcp");
        version_binary(&daemon, "0.20.83");
        version_binary(&mcp, "0.1.38");
        let mut inventory = Inventory::load(root.join("inventory.json"), "linux-x64").unwrap();
        for path in [&daemon, &mcp] {
            inventory.record("managed-binary", path, Map::new());
        }
        let catalog = DaemonCatalogBuild {
            pid: 44,
            version: "0.36.5".to_string(),
        };
        for catalog in [Some(&catalog), None] {
            let targets =
                discover_managed_upgrade_targets(&inventory, &root.join("ck"), catalog).unwrap();
            assert_eq!(targets[0].installed_version, "0.20.83");
        }
    }

    #[cfg(unix)]
    #[test]
    fn discovery_reads_archive_digest_for_currency_not_binary_digest() {
        let root = fixture_dir("currency-digest");
        let ck = root.join("ck");
        let daemon = root.join("ck-subc");
        let mcp = root.join("ck-subc-mcp");
        version_binary(&ck, "1.0.0");
        version_binary(&daemon, "1.0.0");
        version_binary(&mcp, "0.1.0");
        let mut inventory =
            Inventory::load(root.join("installer-manifest.json"), "linux-x64").expect("inventory");
        let mut fields = Map::new();
        fields.insert("sha256".to_string(), Value::String("ab".repeat(32)));
        fields.insert("archive_sha256".to_string(), Value::String("cd".repeat(32)));
        inventory.record("managed-binary", &mcp, fields);
        inventory.record("managed-binary", &daemon, Map::new());

        let targets = discover_managed_upgrade_targets(
            &inventory,
            &ck,
            Some(&DaemonCatalogBuild {
                pid: 44,
                version: "1.0.0".to_string(),
            }),
        )
        .expect("discover");
        let mcp_target = targets
            .iter()
            .find(|item| item.target == upgrade_target("ck-subc-mcp"))
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

    /// A segment of `.`-separated numeric text is only a version component
    /// when it is non-empty ASCII digits: an empty segment ("1..2") or a
    /// non-ASCII digit (Arabic-Indic "١" below) must refuse, because the
    /// accepted text is recorded and rendered as the installed version.
    #[test]
    fn version_output_with_an_empty_segment_is_refused() {
        assert!(
            version_from_output(b"ck 1..2").is_err(),
            "an empty minor segment is not a version"
        );
        assert!(
            version_from_output(b"ck 1.2.").is_err(),
            "an empty patch segment is not a version"
        );
    }

    #[test]
    fn version_output_with_non_ascii_digits_is_refused() {
        assert!(
            version_from_output("ck ١.٢.٣".as_bytes()).is_err(),
            "non-ASCII digits are not a version"
        );
        assert!(
            version_from_output("ck 1.٢.3".as_bytes()).is_err(),
            "a single non-ASCII digit segment is not a version"
        );
    }

    #[test]
    fn release_versions_preserve_prerelease_and_build_suffixes() {
        assert_eq!(
            version_from_output(b"ck 0.18.0-rc.1+build.7").unwrap(),
            "0.18.0-rc.1+build.7"
        );
    }

    #[test]
    fn version_output_accepts_plain_and_prerelease_versions() {
        assert_eq!(
            version_from_output(b"ck 1.2.3").expect("plain version"),
            "1.2.3"
        );
        assert_eq!(
            version_from_output(b"ck 1.2.3-rc.1").expect("prerelease version"),
            "1.2.3-rc.1"
        );
    }

    #[test]
    fn missing_aft_archive_is_typed_release_incomplete() {
        let aft = upgrade_target("ck-aft");
        let target = ManagedUpgradeTarget {
            target: aft,
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
            aft.label().to_string(),
            super::super::update_cache::CachedRelease {
                version: "2.0.0".to_string(),
                sha256: None,
                reports_release_version: true,
            },
        );
        let observed = observed_upgrade_targets(&metadata, &[target], Ok(BTreeSet::new()), None);
        assert!(matches!(
            observed.release(aft),
            super::super::model::ReleaseAvailability::Incomplete { .. }
        ));
    }

    #[test]
    fn upgrade_planner_uses_the_artifact_sources_same_signed_index_generation() {
        let mut index_components = BTreeMap::new();
        index_components.insert(
            "core".to_string(),
            super::super::release_index::IndexComponent {
                release: "subc-core-v0.17.20".to_string(),
                version: Some("0.17.20".to_string()),
                requires_core: None,
                assets: BTreeMap::new(),
            },
        );
        index_components.insert(
            "aft".to_string(),
            super::super::release_index::IndexComponent {
                release: "v1.0.0".to_string(),
                version: Some("1.0.0".to_string()),
                requires_core: Some("0.17.20".to_string()),
                assets: BTreeMap::new(),
            },
        );
        let index = ReleaseIndex {
            schema: 1,
            channel: "alpha".to_string(),
            generated_at_ms: 1,
            components: index_components,
        };
        let mut artifacts = ReleaseArtifactSource::from_index(index, AlphaTarget::LinuxX64);
        artifacts.ensure_index().expect("fixture index");
        let planning_index = artifacts.cloned_index().expect("same cached index");

        let daemon = upgrade_target("ck-subc");
        let aft = upgrade_target("ck-aft");
        let discovered = [
            ManagedUpgradeTarget {
                target: daemon,
                destination: PathBuf::from("/managed/ck-subc"),
                installed_version: "0.17.19".to_string(),
                installed_archive_sha256: Some("old-daemon".to_string()),
            },
            ManagedUpgradeTarget {
                target: aft,
                destination: PathBuf::from("/managed/ck-aft"),
                installed_version: "0.9.0".to_string(),
                installed_archive_sha256: Some("old-aft".to_string()),
            },
        ];
        let mut metadata = UpdateMetadata {
            format_version: super::super::update_cache::UPDATE_CACHE_FORMAT_VERSION,
            checked_at_unix_secs: 1,
            targets: BTreeMap::new(),
        };
        for (target, version, digest) in
            [(daemon, "0.17.20", "new-daemon"), (aft, "1.0.0", "new-aft")]
        {
            metadata.targets.insert(
                target.label().to_string(),
                super::super::update_cache::CachedRelease {
                    version: version.to_string(),
                    sha256: Some(digest.to_string()),
                    reports_release_version: true,
                },
            );
        }
        let observed = observed_upgrade_targets(
            &metadata,
            &discovered,
            Ok(BTreeSet::from(["aft".to_string()])),
            Some(&planning_index),
        );
        let plan = plan_upgrade(&observed);

        assert_eq!(
            observed
                .requires_core
                .get(&Component::Aft)
                .map(String::as_str),
            Some("0.17.20")
        );
        assert!(plan.operations.iter().any(|operation| matches!(
            operation,
            super::super::model::UpgradeOperation::DownloadAndVerify { target } if *target == aft
        )));
        assert!(!plan.outcomes.iter().any(|outcome| matches!(
            outcome,
            super::super::model::PlanOutcome::TargetRefused { reason, .. } if reason.contains("aft")
        )));
    }

    #[cfg(unix)]
    #[test]
    fn initiate_module_restart_uses_module_id_not_binary_label() {
        let root = fixture_dir("initiate-restart-module-id");
        let aft = upgrade_target("ck-aft");
        let ck = root.join("ck");
        write_executable(
            &ck,
            r#"#!/bin/sh
if [ "$1" = "module" ] && [ "$2" = "restart" ]; then
    if [ "$3" = "aft" ]; then
        echo "restart initiated"
        exit 0
    else
        echo "unknown_module — module_id '$3' is not supervised" >&2
        exit 1
    fi
fi
exit 1
"#
            .as_bytes(),
        );

        let aft_path = root.join("ck-aft");
        write_executable(&aft_path, b"#!/bin/sh\necho 'ck-aft 1.0.0'\n");

        let mut inventory =
            Inventory::load(root.join("installer-manifest.json"), "linux-x64").expect("inventory");
        inventory.record("managed-binary", &aft_path, Map::new());

        let mut backend = SystemUpgradeBackend {
            platform: AlphaTarget::LinuxX64,
            targets: [(
                aft.label().to_string(),
                ManagedUpgradeTarget {
                    target: aft,
                    destination: aft_path,
                    installed_version: "1.0.0".to_string(),
                    installed_archive_sha256: None,
                },
            )]
            .into_iter()
            .collect(),
            executable: ck,
            subc: None,
            assets: ReleaseUpgradeAssetFetcher::from_index(ReleaseIndex {
                schema: 1,
                channel: "alpha".to_string(),
                generated_at_ms: 0,
                components: BTreeMap::new(),
            }),
            inventory,
            prepared: BTreeMap::new(),
            activated_inodes: BTreeMap::new(),
            rollback_paths: BTreeMap::new(),
            rollback_archive_sha256: BTreeMap::new(),
            replaced_images: BTreeMap::new(),
            expected_versions: [(aft.label().to_string(), "2.0.0".to_string())]
                .into_iter()
                .collect(),
            planned_from: BTreeMap::new(),
            supervised_modules: ["aft".to_string()].into_iter().collect(),
        };

        let result = backend.initiate_module_restart(aft, Duration::from_secs(30));
        assert!(result.is_ok(), "initiate_module_restart failed: {result:?}");
    }

    /// The completion line is the planner's transition, not the binary's
    /// self-report: ck-subc-mcp prints its crate version 0.1.0, the planner
    /// leaves `from` empty for it, and the line must say "→ release", exactly
    /// as the dry-run did. The two lines are rendered by different code
    /// paths, so agreement between them is a test, not a consequence.
    #[cfg(unix)]
    #[test]
    fn completion_line_follows_the_planned_from_not_the_self_report() {
        let mcp = upgrade_target("ck-subc-mcp");
        let root = ScratchDir::new("completion-line");
        let inventory =
            Inventory::load(root.join("installer-manifest.json"), "linux-x64").expect("inventory");
        let mut backend = SystemUpgradeBackend {
            platform: AlphaTarget::LinuxX64,
            targets: [(
                mcp.label().to_string(),
                ManagedUpgradeTarget {
                    target: mcp,
                    destination: root.join("ck-subc-mcp"),
                    installed_version: "0.1.0".to_string(),
                    installed_archive_sha256: None,
                },
            )]
            .into_iter()
            .collect(),
            executable: root.join("ck"),
            subc: None,
            assets: ReleaseUpgradeAssetFetcher::from_index(ReleaseIndex {
                schema: 1,
                channel: "alpha".to_string(),
                generated_at_ms: 0,
                components: BTreeMap::new(),
            }),
            inventory,
            prepared: BTreeMap::new(),
            activated_inodes: BTreeMap::new(),
            rollback_paths: BTreeMap::new(),
            rollback_archive_sha256: BTreeMap::new(),
            replaced_images: BTreeMap::new(),
            expected_versions: [(mcp.label().to_string(), "0.17.36".to_string())]
                .into_iter()
                .collect(),
            planned_from: BTreeMap::new(),
            supervised_modules: BTreeSet::new(),
        };

        // Without the planner's word, the line falls back to the self-report.
        assert_eq!(
            backend.completion_line(mcp),
            "upgraded ck-subc-mcp 0.1.0 → 0.17.36, restarted"
        );
        backend.set_planned_from(mcp, String::new());
        assert_eq!(
            backend.completion_line(mcp),
            "upgraded ck-subc-mcp → release 0.17.36, restarted"
        );
    }
}
