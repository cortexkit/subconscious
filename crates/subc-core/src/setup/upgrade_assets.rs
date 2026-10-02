use std::{
    fs,
    path::{Path, PathBuf},
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};

use super::{
    model::{AlphaTarget, UpgradeTarget},
    release_index::{self, ReleaseIndex},
    update_check::upgrade_target_index_path,
};

/// Archive and binary names derived from the upgrade target and host tuple.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpgradeAssetNames {
    pub archive: String,
    pub binary: String,
}

pub fn convention_asset_names(target: UpgradeTarget, platform: AlphaTarget) -> UpgradeAssetNames {
    UpgradeAssetNames {
        archive: format!("{}-{}.zip", target.label(), platform.label()),
        binary: platform_binary(target.label()),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpgradeAssetError {
    ReleaseIncomplete {
        missing_asset: String,
    },
    DigestMismatch {
        asset: String,
        expected: String,
        actual: String,
    },
    Extraction {
        asset: String,
        reason: String,
    },
    Io {
        asset: String,
        reason: String,
    },
}

impl std::fmt::Display for UpgradeAssetError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReleaseIncomplete { missing_asset } => {
                write!(
                    formatter,
                    "release-incomplete: missing asset {missing_asset}"
                )
            }
            Self::DigestMismatch {
                asset,
                expected,
                actual,
            } => write!(
                formatter,
                "refusal: SHA-256 mismatch for {asset}: expected {expected}, downloaded {actual}"
            ),
            Self::Extraction { asset, reason } => {
                write!(formatter, "refusal: could not extract {asset}: {reason}")
            }
            Self::Io { asset, reason } => write!(formatter, "refusal: {asset}: {reason}"),
        }
    }
}

impl std::error::Error for UpgradeAssetError {}

/// A downloaded candidate remains in its private workspace until the executor
/// has made a rollback copy and is ready to atomically replace its destination.
#[derive(Clone, Debug)]
pub struct PreparedUpgradeAsset {
    pub names: UpgradeAssetNames,
    pub candidate: PathBuf,
    /// Digest of the verified zip. Currency records this, not the extracted binary.
    pub archive_sha256: String,
    workspace: PathBuf,
}

impl PreparedUpgradeAsset {
    pub fn cleanup(self) {
        let _ = fs::remove_dir_all(self.workspace);
    }
}

pub trait UpgradeAssetFetcher {
    /// Download the archive for `target` on `platform` and return the expected sha256.
    fn fetch_archive(
        &mut self,
        target: UpgradeTarget,
        platform: AlphaTarget,
        destination: &Path,
    ) -> Result<String, UpgradeAssetError>;
}

/// Downloads the archive URL named by a previously fetched signed index.
pub struct ReleaseUpgradeAssetFetcher {
    index: ReleaseIndex,
}

impl ReleaseUpgradeAssetFetcher {
    pub fn from_index(index: ReleaseIndex) -> Self {
        Self { index }
    }

    pub fn accepts_reported_version(&self, target: UpgradeTarget, platform: AlphaTarget) -> bool {
        let (component, binary) = upgrade_target_index_path(target);
        self.index
            .components
            .get(component)
            .and_then(|entry| entry.assets.get(platform.label()))
            .and_then(|assets| assets.get(binary))
            .is_some_and(|asset| asset.reports.is_none())
    }
}

impl UpgradeAssetFetcher for ReleaseUpgradeAssetFetcher {
    fn fetch_archive(
        &mut self,
        target: UpgradeTarget,
        platform: AlphaTarget,
        destination: &Path,
    ) -> Result<String, UpgradeAssetError> {
        let (component, binary) = upgrade_target_index_path(target);
        let missing = || UpgradeAssetError::ReleaseIncomplete {
            missing_asset: format!("{}-{}.zip", binary, platform.label()),
        };
        let asset = self
            .index
            .components
            .get(component)
            .and_then(|entry| entry.assets.get(platform.label()))
            .and_then(|assets| assets.get(binary))
            .ok_or_else(missing)?;
        release_index::download(&asset.url, destination).map_err(|_| missing())?;
        Ok(asset.sha256.to_ascii_lowercase())
    }
}

/// Download the archive named by the index and verify it against the index
/// digest. Extraction is after the digest check: a corrupt archive must never
/// reach an extractor or a managed destination.
pub fn prepare_upgrade_asset<F: UpgradeAssetFetcher>(
    fetcher: &mut F,
    target: UpgradeTarget,
    platform: AlphaTarget,
) -> Result<PreparedUpgradeAsset, UpgradeAssetError> {
    let names = convention_asset_names(target, platform);
    let workspace = WorkspaceGuard(temporary_workspace(target)?);
    let archive = workspace.0.join(&names.archive);

    let expected = fetcher.fetch_archive(target, platform, &archive)?;
    let actual = sha256_file(&archive).map_err(|reason| UpgradeAssetError::Io {
        asset: names.archive.clone(),
        reason,
    })?;
    if actual != expected {
        return Err(UpgradeAssetError::DigestMismatch {
            asset: names.archive,
            expected,
            actual,
        });
    }

    let extracted = workspace.0.join("extracted");
    extract(&archive, &extracted).map_err(|reason| UpgradeAssetError::Extraction {
        asset: names.archive.clone(),
        reason,
    })?;
    let candidate = extracted.join(&names.binary);
    if !candidate.is_file() {
        return Err(UpgradeAssetError::Extraction {
            asset: names.archive,
            reason: format!("archive did not contain {} at its root", names.binary),
        });
    }
    Ok(PreparedUpgradeAsset {
        names,
        candidate,
        archive_sha256: expected,
        workspace: workspace.hand_off(),
    })
}

/// Removes the temporary workspace on drop so every early return between
/// its creation and the handoff to a `PreparedUpgradeAsset` still cleans
/// up. The handoff disarms the guard: from then on
/// `PreparedUpgradeAsset::cleanup` owes the removal.
struct WorkspaceGuard(PathBuf);

impl WorkspaceGuard {
    fn hand_off(self) -> PathBuf {
        let workspace = self.0.clone();
        std::mem::forget(self);
        workspace
    }
}

impl Drop for WorkspaceGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn temporary_workspace(target: UpgradeTarget) -> Result<PathBuf, UpgradeAssetError> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| UpgradeAssetError::Io {
            asset: target.label().to_string(),
            reason: format!("clock before Unix epoch: {error}"),
        })?
        .as_nanos();
    let workspace = std::env::temp_dir().join(format!(
        "ck-upgrade-{}-{}-{nonce}",
        target.label(),
        process::id()
    ));
    fs::create_dir_all(&workspace).map_err(|error| UpgradeAssetError::Io {
        asset: target.label().to_string(),
        reason: format!(
            "could not create temporary workspace {}: {error}",
            workspace.display()
        ),
    })?;
    Ok(workspace)
}

fn extract(archive: &Path, destination: &Path) -> Result<(), String> {
    let program = if cfg!(windows) {
        "powershell.exe"
    } else {
        "unzip"
    };
    let output = extract_command(archive, destination)
        .output()
        .map_err(|error| format!("could not run {program}: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn extract_command(archive: &Path, destination: &Path) -> Command {
    if cfg!(windows) {
        windows_extract_command(archive, destination)
    } else {
        unix_extract_command(archive, destination)
    }
}

/// Fixed PowerShell script text: both paths reach the child through its
/// environment, never through interpolation, so an apostrophe in a profile
/// path (a user named `O'Neil`) cannot end a `'...'` segment early and a
/// path containing `'; ...` cannot run as PowerShell.
const EXPAND_ARCHIVE_SCRIPT: &str =
    "Expand-Archive -LiteralPath $env:CK_ARCHIVE -DestinationPath $env:CK_DEST -Force";

fn windows_extract_command(archive: &Path, destination: &Path) -> Command {
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            EXPAND_ARCHIVE_SCRIPT,
        ])
        .env("CK_ARCHIVE", archive.to_string_lossy().into_owned())
        .env("CK_DEST", destination.to_string_lossy().into_owned());
    command
}

fn unix_extract_command(archive: &Path, destination: &Path) -> Command {
    let mut command = Command::new("unzip");
    command.args([
        "-q".to_string(),
        archive.to_string_lossy().into_owned(),
        "-d".to_string(),
        destination.to_string_lossy().into_owned(),
    ]);
    command
}

fn platform_binary(binary: &str) -> String {
    if cfg!(windows) {
        format!("{binary}.exe")
    } else {
        binary.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::setup::{components::upgrade_roster, model::Component};

    fn upgrade_target(binary: &str) -> UpgradeTarget {
        upgrade_roster(Component::ALL)
            .into_iter()
            .find(|target| target.label() == binary)
            .unwrap_or_else(|| panic!("missing upgrade target {binary}"))
    }

    #[derive(Default)]
    struct MemoryFetcher {
        assets: BTreeMap<String, Vec<u8>>,
        digests: BTreeMap<String, String>,
        calls: Vec<String>,
        destinations: Vec<PathBuf>,
    }

    impl UpgradeAssetFetcher for MemoryFetcher {
        fn fetch_archive(
            &mut self,
            target: UpgradeTarget,
            platform: AlphaTarget,
            destination: &Path,
        ) -> Result<String, UpgradeAssetError> {
            let names = convention_asset_names(target, platform);
            self.calls.push(names.archive.clone());
            self.destinations.push(destination.to_path_buf());
            let bytes = self.assets.get(&names.archive).ok_or_else(|| {
                UpgradeAssetError::ReleaseIncomplete {
                    missing_asset: names.archive.clone(),
                }
            })?;
            fs::write(destination, bytes).map_err(|error| UpgradeAssetError::Io {
                asset: names.archive.clone(),
                reason: error.to_string(),
            })?;
            self.digests.get(&names.archive).cloned().ok_or({
                UpgradeAssetError::ReleaseIncomplete {
                    missing_asset: names.archive,
                }
            })
        }
    }

    #[test]
    fn asset_names_are_directly_derived_for_every_alpha_tuple() {
        for platform in AlphaTarget::ALL {
            let names = convention_asset_names(upgrade_target("ck-aft"), platform);
            assert_eq!(names.archive, format!("ck-aft-{}.zip", platform.label()));
        }
    }

    #[test]
    fn missing_archive_is_a_typed_refusal_that_names_the_exact_asset() {
        let mut fetcher = MemoryFetcher::default();
        let names = convention_asset_names(upgrade_target("ck-aft"), AlphaTarget::LinuxX64);

        let error = prepare_upgrade_asset(
            &mut fetcher,
            upgrade_target("ck-aft"),
            AlphaTarget::LinuxX64,
        )
        .expect_err("missing archive must refuse");
        assert_eq!(
            error,
            UpgradeAssetError::ReleaseIncomplete {
                missing_asset: names.archive.clone()
            }
        );
        assert_eq!(fetcher.calls, vec![names.archive]);
    }

    #[test]
    fn missing_index_digest_is_a_typed_refusal_that_names_the_exact_asset() {
        let mut fetcher = MemoryFetcher::default();
        let names = convention_asset_names(upgrade_target("ck-aft"), AlphaTarget::LinuxX64);
        fetcher
            .assets
            .insert(names.archive.clone(), b"archive".to_vec());

        let error = prepare_upgrade_asset(
            &mut fetcher,
            upgrade_target("ck-aft"),
            AlphaTarget::LinuxX64,
        )
        .expect_err("missing digest must refuse");
        assert_eq!(
            error,
            UpgradeAssetError::ReleaseIncomplete {
                missing_asset: names.archive.clone()
            }
        );
        assert_eq!(fetcher.calls, vec![names.archive]);
    }

    /// The `-Command` argument of a Windows command, with the command's
    /// environment as name/value pairs.
    fn script_and_env(command: &Command) -> (String, BTreeMap<String, String>) {
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let position = args
            .iter()
            .position(|arg| arg == "-Command")
            .expect("a powershell command carries -Command");
        let env = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value
                        .map(|value| value.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                )
            })
            .collect();
        (args[position + 1].clone(), env)
    }

    /// A Windows profile can put an apostrophe in the temp paths handed to
    /// `Expand-Archive` (user name `O'Neil`), and an interpolated `'...'`
    /// value then ends the quoted segment early. The script text must be
    /// FIXED and both paths must reach the child through its environment.
    #[test]
    fn windows_expand_archive_keeps_paths_out_of_the_script_text() {
        let archive = PathBuf::from(
            "C:\\Users\\O'Neil\\AppData\\Local\\Temp\\ck.zip'; Write-Output INJECTED; '",
        );
        let destination = PathBuf::from("C:\\Users\\O'Neil\\AppData\\Local\\Temp\\extracted");
        let command = windows_extract_command(&archive, &destination);
        assert_eq!(command.get_program().to_string_lossy(), "powershell.exe");
        let (script, env) = script_and_env(&command);
        for value in [
            archive.to_string_lossy().into_owned(),
            destination.to_string_lossy().into_owned(),
        ] {
            assert!(
                !script.contains(&value),
                "script must not interpolate a value: {script}"
            );
        }
        assert!(
            !script.contains("INJECTED"),
            "no value byte may reach the script text: {script}"
        );
        assert_eq!(
            env.get("CK_ARCHIVE").map(String::as_str),
            Some(archive.to_string_lossy().as_ref())
        );
        assert_eq!(
            env.get("CK_DEST").map(String::as_str),
            Some(destination.to_string_lossy().as_ref())
        );
    }

    /// A failed preparation must not leave its temporary workspace behind:
    /// every early return between creating it and handing it to the caller
    /// still owes a `remove_dir_all`.
    #[test]
    fn a_failed_preparation_removes_its_temporary_workspace() {
        let mut fetcher = MemoryFetcher::default();
        let names = convention_asset_names(upgrade_target("ck-subc-mcp"), AlphaTarget::LinuxX64);
        fetcher
            .assets
            .insert(names.archive.clone(), b"corrupted".to_vec());
        fetcher
            .digests
            .insert(names.archive.clone(), "0".repeat(64));

        let error = prepare_upgrade_asset(
            &mut fetcher,
            upgrade_target("ck-subc-mcp"),
            AlphaTarget::LinuxX64,
        )
        .expect_err("digest mismatch must refuse");
        assert!(matches!(error, UpgradeAssetError::DigestMismatch { .. }));
        let workspace = fetcher.destinations[0]
            .parent()
            .expect("the archive lives inside the workspace")
            .to_path_buf();
        assert!(
            !workspace.exists(),
            "a refused preparation leaks its workspace: {}",
            workspace.display()
        );
    }

    #[test]
    fn corrupted_download_refuses_before_extraction() {
        let mut fetcher = MemoryFetcher::default();
        let names = convention_asset_names(upgrade_target("ck-subc-mcp"), AlphaTarget::LinuxX64);
        fetcher
            .assets
            .insert(names.archive.clone(), b"corrupted".to_vec());
        fetcher
            .digests
            .insert(names.archive.clone(), "0".repeat(64));

        let error = prepare_upgrade_asset(
            &mut fetcher,
            upgrade_target("ck-subc-mcp"),
            AlphaTarget::LinuxX64,
        )
        .expect_err("digest mismatch must refuse");
        assert!(matches!(error, UpgradeAssetError::DigestMismatch { .. }));
        assert_eq!(fetcher.calls, vec![names.archive]);
    }
}
