use std::{
    fs,
    path::{Path, PathBuf},
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};

use super::model::{AlphaTarget, UpgradeTarget};

/// The two release names are derived together so an upgrade cannot accidentally
/// pair an archive with a digest intended for a different host binary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpgradeAssetNames {
    pub archive: String,
    pub sidecar: String,
    pub binary: String,
}

pub fn convention_asset_names(target: UpgradeTarget, platform: AlphaTarget) -> UpgradeAssetNames {
    let archive = format!("{}-{}.zip", target.label(), platform.label());
    UpgradeAssetNames {
        sidecar: format!("{archive}.sha256"),
        binary: platform_binary(target.label()),
        archive,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpgradeAssetError {
    ReleaseIncomplete {
        missing_asset: String,
    },
    DigestSidecar {
        asset: String,
        reason: String,
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
            Self::DigestSidecar { asset, reason } => {
                write!(
                    formatter,
                    "refusal: invalid digest sidecar {asset}: {reason}"
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
    workspace: PathBuf,
}

impl PreparedUpgradeAsset {
    pub fn cleanup(self) {
        let _ = fs::remove_dir_all(self.workspace);
    }
}

pub trait UpgradeAssetFetcher {
    fn download(
        &mut self,
        target: UpgradeTarget,
        asset: &str,
        destination: &Path,
    ) -> Result<(), String>;
}

/// Release downloads use separate repository bases, while their asset names stay
/// convention-derived. The base overrides make a release mirror testable without
/// adding a manifest or target-to-asset mapping table.
pub struct ReleaseUpgradeAssetFetcher {
    subc_base_url: String,
    aft_base_url: String,
}

impl ReleaseUpgradeAssetFetcher {
    pub fn from_environment() -> Self {
        Self {
            subc_base_url: std::env::var("CK_RELEASE_BASE_URL").unwrap_or_else(|_| {
                "https://github.com/cortexkit/subconscious/releases/latest/download".to_string()
            }),
            aft_base_url: std::env::var("CK_AFT_RELEASE_BASE_URL").unwrap_or_else(|_| {
                "https://github.com/cortexkit/aft/releases/latest/download".to_string()
            }),
        }
    }

    fn release_base(&self, target: UpgradeTarget) -> &str {
        match target {
            UpgradeTarget::Aft => &self.aft_base_url,
            UpgradeTarget::SubcMcp | UpgradeTarget::Daemon | UpgradeTarget::Ck => {
                &self.subc_base_url
            }
        }
    }
}

impl UpgradeAssetFetcher for ReleaseUpgradeAssetFetcher {
    fn download(
        &mut self,
        target: UpgradeTarget,
        asset: &str,
        destination: &Path,
    ) -> Result<(), String> {
        let url = format!(
            "{}/{asset}",
            self.release_base(target).trim_end_matches('/')
        );
        let destination = destination.to_string_lossy().into_owned();
        let (program, args) = if cfg!(windows) {
            (
                "powershell.exe",
                vec![
                    "-NoProfile".to_string(),
                    "-NonInteractive".to_string(),
                    "-Command".to_string(),
                    format!(
                        "Invoke-WebRequest -Uri '{url}' -OutFile '{destination}' -UseBasicParsing"
                    ),
                ],
            )
        } else {
            (
                "curl",
                vec![
                    "--fail".to_string(),
                    "--location".to_string(),
                    "--silent".to_string(),
                    "--show-error".to_string(),
                    "--output".to_string(),
                    destination,
                    url,
                ],
            )
        };
        let output = Command::new(program)
            .args(args)
            .output()
            .map_err(|error| format!("could not run {program}: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }
}

/// Download a convention-derived archive and exactly its matching sidecar.
/// Extraction is deliberately after the digest check: a corrupt archive must
/// never reach an extractor or a managed destination.
pub fn prepare_upgrade_asset<F: UpgradeAssetFetcher>(
    fetcher: &mut F,
    target: UpgradeTarget,
    platform: AlphaTarget,
) -> Result<PreparedUpgradeAsset, UpgradeAssetError> {
    let names = convention_asset_names(target, platform);
    let workspace = temporary_workspace(target)?;
    let archive = workspace.join(&names.archive);
    let sidecar = workspace.join(&names.sidecar);

    fetcher
        .download(target, &names.archive, &archive)
        .map_err(|_| UpgradeAssetError::ReleaseIncomplete {
            missing_asset: names.archive.clone(),
        })?;
    fetcher
        .download(target, &names.sidecar, &sidecar)
        .map_err(|_| UpgradeAssetError::ReleaseIncomplete {
            missing_asset: names.sidecar.clone(),
        })?;

    let expected = fs::read_to_string(&sidecar)
        .map_err(|error| UpgradeAssetError::Io {
            asset: names.sidecar.clone(),
            reason: error.to_string(),
        })
        .and_then(|contents| {
            parse_sidecar(&contents, &names.archive).map_err(|reason| {
                UpgradeAssetError::DigestSidecar {
                    asset: names.sidecar.clone(),
                    reason,
                }
            })
        })?;
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

    let extracted = workspace.join("extracted");
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
        workspace,
    })
}

pub fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn parse_sidecar(contents: &str, archive_name: &str) -> Result<String, String> {
    let mut lines = contents.lines();
    let line = lines.next().ok_or_else(|| "sidecar is empty".to_string())?;
    if lines.next().is_some() {
        return Err("sidecar has more than one digest record".to_string());
    }
    let mut fields = line.split_whitespace();
    let digest = fields
        .next()
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| "sidecar has no SHA-256 digest".to_string())?;
    if let Some(recorded_name) = fields.next() {
        if recorded_name.trim_start_matches('*') != archive_name || fields.next().is_some() {
            return Err(format!("sidecar does not name {archive_name}"));
        }
    }
    Ok(digest.to_ascii_lowercase())
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
    let (program, args) = if cfg!(windows) {
        (
            "powershell.exe",
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                format!(
                    "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
                    archive.display(),
                    destination.display()
                ),
            ],
        )
    } else {
        (
            "unzip",
            vec![
                "-q".to_string(),
                archive.to_string_lossy().into_owned(),
                "-d".to_string(),
                destination.to_string_lossy().into_owned(),
            ],
        )
    };
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("could not run {program}: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
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

    #[derive(Default)]
    struct MemoryFetcher {
        assets: BTreeMap<String, Vec<u8>>,
        calls: Vec<String>,
    }

    impl UpgradeAssetFetcher for MemoryFetcher {
        fn download(
            &mut self,
            _target: UpgradeTarget,
            asset: &str,
            destination: &Path,
        ) -> Result<(), String> {
            self.calls.push(asset.to_string());
            let bytes = self
                .assets
                .get(asset)
                .ok_or_else(|| "not found".to_string())?;
            fs::write(destination, bytes).map_err(|error| error.to_string())
        }
    }

    #[test]
    fn asset_names_are_directly_derived_for_every_alpha_tuple() {
        for platform in [
            AlphaTarget::DarwinArm64,
            AlphaTarget::LinuxX64,
            AlphaTarget::WindowsX64,
        ] {
            let names = convention_asset_names(UpgradeTarget::Aft, platform);
            assert_eq!(names.archive, format!("ck-aft-{}.zip", platform.label()));
            assert_eq!(
                names.sidecar,
                format!("ck-aft-{}.zip.sha256", platform.label())
            );
        }
    }

    #[test]
    fn missing_archive_is_a_typed_refusal_that_names_the_exact_asset() {
        let mut fetcher = MemoryFetcher::default();
        let names = convention_asset_names(UpgradeTarget::Aft, AlphaTarget::LinuxX64);

        let error = prepare_upgrade_asset(&mut fetcher, UpgradeTarget::Aft, AlphaTarget::LinuxX64)
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
    fn missing_sidecar_is_a_typed_refusal_that_names_the_exact_asset() {
        let mut fetcher = MemoryFetcher::default();
        let names = convention_asset_names(UpgradeTarget::Aft, AlphaTarget::LinuxX64);
        fetcher
            .assets
            .insert(names.archive.clone(), b"archive".to_vec());

        let error = prepare_upgrade_asset(&mut fetcher, UpgradeTarget::Aft, AlphaTarget::LinuxX64)
            .expect_err("missing sidecar must refuse");
        assert_eq!(
            error,
            UpgradeAssetError::ReleaseIncomplete {
                missing_asset: names.sidecar.clone()
            }
        );
        assert_eq!(fetcher.calls, vec![names.archive, names.sidecar]);
    }

    #[test]
    fn corrupted_download_refuses_before_extraction() {
        let mut fetcher = MemoryFetcher::default();
        let names = convention_asset_names(UpgradeTarget::SubcMcp, AlphaTarget::LinuxX64);
        fetcher
            .assets
            .insert(names.archive.clone(), b"corrupted".to_vec());
        fetcher.assets.insert(
            names.sidecar.clone(),
            format!("{}  {}\n", "0".repeat(64), names.archive).into_bytes(),
        );

        let error =
            prepare_upgrade_asset(&mut fetcher, UpgradeTarget::SubcMcp, AlphaTarget::LinuxX64)
                .expect_err("digest mismatch must refuse");
        assert!(matches!(error, UpgradeAssetError::DigestMismatch { .. }));
        assert_eq!(fetcher.calls, vec![names.archive, names.sidecar]);
    }
}
