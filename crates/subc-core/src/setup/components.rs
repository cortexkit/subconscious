use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::{
    config,
    inventory::Inventory,
    model::{AlphaTarget, Component, ReleaseAvailability, ReleaseResolutionStrategy},
};

pub trait ArtifactSource {
    fn install(
        &mut self,
        component: Component,
        binary: &str,
        destination: &Path,
    ) -> Result<String, String>;

    fn expected_version(&mut self, component: Component) -> Result<String, String>;
}

/// Downloads only convention-derived archives and verifies each archive against
/// its matching sidecar before extracting the binary into the managed home.
pub struct ReleaseArtifactSource {
    target: AlphaTarget,
    api_base: String,
    release_bases: BTreeMap<Component, String>,
    manifests: BTreeMap<Component, ReleaseManifest>,
}

#[derive(Clone, Debug, Deserialize)]
struct ReleaseManifest {
    tag_name: String,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    #[allow(
        dead_code,
        reason = "prereleases intentionally qualify for tag-pattern resolution"
    )]
    prerelease: bool,
    #[serde(default)]
    created_at: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
}

impl ReleaseArtifactSource {
    pub fn current() -> Self {
        let mut release_bases = BTreeMap::new();
        for component in Component::ALL {
            let key = if component == Component::Core {
                "CK_RELEASE_BASE_URL".to_string()
            } else {
                format!(
                    "CK_{}_RELEASE_BASE_URL",
                    component.label().to_ascii_uppercase().replace('-', "_")
                )
            };
            release_bases.insert(
                component,
                std::env::var(key).unwrap_or_else(|_| {
                    match component.release_resolution_strategy() {
                        ReleaseResolutionStrategy::Latest => format!(
                            "https://github.com/cortexkit/{}/releases/latest/download",
                            component.repository()
                        ),
                        ReleaseResolutionStrategy::TagPrefix(_) => format!(
                            "https://github.com/cortexkit/{}/releases/download",
                            component.repository()
                        ),
                    }
                }),
            );
        }
        Self {
            target: if cfg!(target_os = "macos") {
                AlphaTarget::DarwinArm64
            } else if cfg!(windows) {
                AlphaTarget::WindowsX64
            } else {
                AlphaTarget::LinuxX64
            },
            api_base: std::env::var("CK_RELEASE_API_BASE_URL")
                .unwrap_or_else(|_| "https://api.github.com".to_string()),
            release_bases,
            manifests: BTreeMap::new(),
        }
    }

    pub fn release_availability(
        &mut self,
        component: Component,
    ) -> Result<ReleaseAvailability, String> {
        let target = self.target;
        let manifest = self.manifest(component)?;
        let assets = manifest
            .assets
            .iter()
            .map(|asset| asset.name.as_str())
            .collect::<BTreeSet<_>>();
        let missing_asset = component_binaries_for_target(component, target)
            .iter()
            .flat_map(|binary| {
                let archive = format!("{}-{}.zip", binary, target.label());
                [archive.clone(), format!("{archive}.sha256")]
            })
            .find(|asset| !assets.contains(asset.as_str()));
        Ok(match missing_asset {
            Some(missing_asset) => ReleaseAvailability::NotYetPublished {
                release_tag: manifest.tag_name.clone(),
                missing_asset,
            },
            None => ReleaseAvailability::Available,
        })
    }

    fn release_base(&self, component: Component, tag: &str) -> String {
        let base = self
            .release_bases
            .get(&component)
            .expect("all setup components have a release base")
            .trim_end_matches('/');
        match component.release_resolution_strategy() {
            ReleaseResolutionStrategy::Latest => base.to_string(),
            ReleaseResolutionStrategy::TagPrefix(_) => format!("{base}/{tag}"),
        }
    }

    fn manifest_url(&self, component: Component) -> String {
        let api_base = self.api_base.trim_end_matches('/');
        match component.release_resolution_strategy() {
            ReleaseResolutionStrategy::Latest => format!(
                "{api_base}/repos/cortexkit/{}/releases/latest",
                component.repository()
            ),
            ReleaseResolutionStrategy::TagPrefix(_) => format!(
                "{api_base}/repos/cortexkit/{}/releases?per_page=100",
                component.repository()
            ),
        }
    }

    fn manifest(&mut self, component: Component) -> Result<&ReleaseManifest, String> {
        if !self.manifests.contains_key(&component) {
            let temporary = temporary_path("release.json");
            let url = self.manifest_url(component);
            download(&url, &temporary)
                .map_err(|error| format!("could not resolve {} release: {error}", component))?;
            let contents = fs::read_to_string(&temporary)
                .map_err(|error| format!("could not read {} release: {error}", component))?;
            let _ = fs::remove_file(&temporary);
            let manifest = match component.release_resolution_strategy() {
                ReleaseResolutionStrategy::Latest => {
                    serde_json::from_str::<ReleaseManifest>(&contents).map_err(|error| {
                        format!("could not parse latest {} release: {error}", component)
                    })?
                }
                ReleaseResolutionStrategy::TagPrefix(prefix) => {
                    let releases = serde_json::from_str::<Vec<ReleaseManifest>>(&contents)
                        .map_err(|error| {
                            format!("could not parse {} release list: {error}", component)
                        })?;
                    newest_matching_release(releases, prefix).unwrap_or_else(|| ReleaseManifest {
                        tag_name: format!("{prefix}*"),
                        assets: Vec::new(),
                        draft: false,
                        prerelease: false,
                        created_at: String::new(),
                    })
                }
            };
            if manifest.tag_name.trim().is_empty() {
                return Err(format!("{component} release has no tag"));
            }
            self.manifests.insert(component, manifest);
        }
        Ok(self
            .manifests
            .get(&component)
            .expect("manifest was inserted for the requested component"))
    }
}

impl ArtifactSource for ReleaseArtifactSource {
    fn install(
        &mut self,
        component: Component,
        binary: &str,
        destination: &Path,
    ) -> Result<String, String> {
        let binary_name = platform_binary(binary);
        let archive_name = format!("{}-{}.zip", binary, self.target.label());
        let sidecar_name = format!("{archive_name}.sha256");
        let temp = std::env::temp_dir().join(format!(
            "ck-setup-{binary}-{}-{}",
            process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| format!("clock before Unix epoch: {error}"))?
                .as_nanos()
        ));
        fs::create_dir_all(&temp).map_err(|error| {
            format!(
                "could not create download directory {}: {error}",
                temp.display()
            )
        })?;
        let archive = temp.join(&archive_name);
        let sidecar = temp.join(&sidecar_name);
        let tag = self.manifest(component)?.tag_name.clone();
        let base = self.release_base(component, &tag);
        download(&format!("{base}/{archive_name}"), &archive)?;
        download(&format!("{base}/{sidecar_name}"), &sidecar)?;
        let expected = parse_sidecar(
            &fs::read_to_string(&sidecar).map_err(|error| {
                format!(
                    "could not read digest sidecar {}: {error}",
                    sidecar.display()
                )
            })?,
            &archive_name,
        )?;
        let actual = digest_file(&archive)?;
        if actual != expected {
            return Err(format!(
                "digest mismatch for {archive_name}: expected {expected} but downloaded {actual}"
            ));
        }
        let extracted = temp.join("extracted");
        extract(&archive, &extracted)?;
        let candidate = extracted.join(&binary_name);
        if !candidate.is_file() {
            return Err(format!(
                "{archive_name} did not contain {binary_name} at its archive root"
            ));
        }
        let parent = destination.parent().ok_or_else(|| {
            format!(
                "managed binary destination {} has no parent",
                destination.display()
            )
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "could not create managed binary directory {}: {error}",
                parent.display()
            )
        })?;
        let temporary = destination.with_extension("setup.tmp");
        fs::copy(&candidate, &temporary).map_err(|error| {
            format!(
                "could not place {binary_name} at {}: {error}",
                destination.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755)).map_err(
                |error| {
                    format!(
                        "could not mark {} executable: {error}",
                        destination.display()
                    )
                },
            )?;
        }
        fs::rename(&temporary, destination).map_err(|error| {
            format!(
                "could not replace managed binary {}: {error}",
                destination.display()
            )
        })?;
        let digest = digest_file(destination)?;
        let _ = fs::remove_dir_all(temp);
        Ok(digest)
    }

    fn expected_version(&mut self, component: Component) -> Result<String, String> {
        let tag = self.manifest(component)?.tag_name.trim();
        let version = tag.trim_start_matches('v');
        if version.is_empty() || version == tag {
            return Err(format!(
                "latest {component} release tag '{tag}' must be v<crate-version>"
            ));
        }
        Ok(version.to_string())
    }
}

pub fn component_binaries(component: Component) -> &'static [&'static str] {
    let target = if cfg!(target_os = "macos") {
        AlphaTarget::DarwinArm64
    } else if cfg!(windows) {
        AlphaTarget::WindowsX64
    } else {
        AlphaTarget::LinuxX64
    };
    component_binaries_for_target(component, target)
}

/// Release asset sets are data, not filesystem discovery, so setup never loses
/// a synapse worker merely because a different worker happens to be installed.
pub fn component_binaries_for_target(
    component: Component,
    target: AlphaTarget,
) -> &'static [&'static str] {
    match (component, target) {
        (Component::Core, _) => &["ck-subc", "ck-subc-mcp"],
        (Component::Aft, _) => &["aft"],
        (Component::Mc, AlphaTarget::DarwinArm64 | AlphaTarget::LinuxX64) => &["ck-mc"],
        (Component::Mc, AlphaTarget::WindowsX64) => &[],
        (Component::Insula, _) => &["ck-insula"],
        (Component::Claustrum, _) => &["ck-claustrum", "ck-auth"],
        (Component::Synapse, AlphaTarget::DarwinArm64) => &[
            "ck-synapse",
            "ck-synapse-opctl",
            "ck-synapse-worker-llama",
            "ck-synapse-worker-mlx",
            "ck-synapse-worker-ane",
            "ck-synapse-worker-decode",
        ],
        (Component::Synapse, AlphaTarget::LinuxX64 | AlphaTarget::WindowsX64) => {
            &["ck-synapse", "ck-synapse-opctl", "ck-synapse-worker-llama"]
        }
    }
}

pub fn is_installed(component: Component, binary_home: &Path, inventory: &Inventory) -> bool {
    component_binaries(component).iter().all(|binary| {
        let path = binary_home.join(platform_binary(binary));
        path.is_file() && inventory.owns_path("managed-binary", &path)
    })
}

pub fn install_component<S: ArtifactSource>(
    component: Component,
    binary_home: &Path,
    inventory: &mut Inventory,
    source: &mut S,
) -> Result<(), String> {
    for binary in component_binaries(component) {
        let destination = binary_home.join(platform_binary(binary));
        if inventory.owns_path("managed-binary", &destination) && destination.is_file() {
            continue;
        }
        if destination.exists() {
            return Err(format!(
                "refusal: managed binary destination {} exists without inventory ownership",
                destination.display()
            ));
        }
        let digest = source.install(component, binary, &destination)?;
        verify_version(&destination, &source.expected_version(component)?)?;
        let mut fields = Map::new();
        fields.insert(
            "component".to_string(),
            Value::String(component.label().to_string()),
        );
        fields.insert("sha256".to_string(), Value::String(digest));
        inventory.record("managed-binary", &destination, fields);
    }
    Ok(())
}

pub fn configure_component(
    component: Component,
    config_path: &Path,
    binary_home: &Path,
    claustrum_key_path: Option<&Path>,
    inventory: &mut Inventory,
) -> Result<Option<config::ConfigChange>, String> {
    let change =
        config::plan_component_with_key(config_path, component, binary_home, claustrum_key_path)
            .map_err(|conflict| {
                format!(
                    "refusal: conflicting user-owned configuration key '{}'; {} was not changed",
                    conflict.key,
                    config_path.display()
                )
            })?;
    if let Some(change) = &change {
        println!("proposed configuration diff:\n{}", change.render_diff());
        config::apply(change)?;
        let mut fields = Map::new();
        fields.insert(
            "component".to_string(),
            Value::String(component.label().to_string()),
        );
        inventory.record("configuration", config_path, fields);
    }
    Ok(change)
}

pub fn configuration_is_correct(
    component: Component,
    config_path: &Path,
    binary_home: &Path,
    claustrum_key_path: Option<&Path>,
) -> Result<bool, String> {
    match config::plan_component_with_key(config_path, component, binary_home, claustrum_key_path) {
        Ok(None) => Ok(true),
        Ok(Some(_)) => Ok(false),
        Err(conflict) => Err(format!("configuration conflict at {}", conflict.key)),
    }
}

fn temporary_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "ck-setup-{name}-{}-{}",
        process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time is after the Unix epoch")
            .as_nanos()
    ))
}

/// The release tag is the expected crate version. Running the placed binary
/// before configuration prevents a valid archive for the wrong release from
/// becoming a supervised module.
fn verify_version(path: &Path, expected: &str) -> Result<(), String> {
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
    let reported = String::from_utf8_lossy(&output.stdout);
    if reported
        .split_whitespace()
        .all(|token| token.trim_start_matches('v') != expected)
    {
        return Err(format!(
            "refusal: {} --version did not report release version {expected}: {reported:?}",
            path.display()
        ));
    }
    Ok(())
}

fn download(url: &str, destination: &Path) -> Result<(), String> {
    let destination = destination.to_string_lossy().into_owned();
    let (program, args) = if cfg!(windows) {
        (
            "powershell.exe",
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                format!("Invoke-WebRequest -Uri '{url}' -OutFile '{destination}' -UseBasicParsing"),
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
                url.to_string(),
            ],
        )
    };
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|error| format!("could not download {url}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("release-incomplete: could not download {url}"))
    }
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
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|error| format!("could not extract {}: {error}", archive.display()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("extraction failed for {}", archive.display()))
    }
}

fn parse_sidecar(contents: &str, archive_name: &str) -> Result<String, String> {
    let mut lines = contents.lines();
    let line = lines
        .next()
        .ok_or_else(|| format!("digest sidecar for {archive_name} is empty"))?;
    if lines.next().is_some() {
        return Err(format!(
            "digest sidecar for {archive_name} has more than one record"
        ));
    }
    let mut fields = line.split_whitespace();
    let digest = fields
        .next()
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| format!("digest sidecar for {archive_name} has no SHA-256 digest"))?;
    if let Some(name) = fields.next() {
        if name.trim_start_matches('*') != archive_name || fields.next().is_some() {
            return Err(format!("digest sidecar does not name {archive_name}"));
        }
    }
    Ok(digest.to_ascii_lowercase())
}

pub fn digest_file(path: &Path) -> Result<String, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("could not hash {}: {error}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn newest_matching_release(
    releases: impl IntoIterator<Item = ReleaseManifest>,
    tag_prefix: &str,
) -> Option<ReleaseManifest> {
    releases
        .into_iter()
        .filter(|release| !release.draft && release.tag_name.starts_with(tag_prefix))
        .max_by(|left, right| left.created_at.cmp(&right.created_at))
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
    use super::*;
    use subc_core::test_support::TestTempDir;

    #[derive(Default)]
    struct FakeSource;

    impl ArtifactSource for FakeSource {
        fn install(
            &mut self,
            _component: Component,
            binary: &str,
            destination: &Path,
        ) -> Result<String, String> {
            fs::write(destination, format!("#!/bin/sh\necho {binary} 1.2.3\n"))
                .map_err(|error| error.to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(destination, fs::Permissions::from_mode(0o755))
                    .map_err(|error| error.to_string())?;
            }
            digest_file(destination)
        }

        fn expected_version(&mut self, _component: Component) -> Result<String, String> {
            Ok("1.2.3".to_string())
        }
    }

    fn fixture_dir(name: &str) -> TestTempDir {
        TestTempDir::new(name)
    }

    #[test]
    fn installed_binaries_are_inventory_owned_and_repeated_install_is_a_noop() {
        let root = fixture_dir("inventory");
        let binary_home = root.join("bin");
        fs::create_dir_all(&binary_home).expect("binary home");
        let mut inventory =
            Inventory::load(root.join("installer-manifest.json"), "linux-x64").expect("inventory");
        let mut source = FakeSource;
        install_component(Component::Core, &binary_home, &mut inventory, &mut source)
            .expect("install core");
        assert!(is_installed(Component::Core, &binary_home, &inventory));
        install_component(Component::Core, &binary_home, &mut inventory, &mut source)
            .expect("repeat core install");
        assert_eq!(inventory.paths_for_kind("managed-binary").len(), 2);
    }

    #[test]
    fn synapse_uses_the_full_declared_platform_asset_sets() {
        assert_eq!(
            component_binaries_for_target(Component::Synapse, AlphaTarget::DarwinArm64),
            [
                "ck-synapse",
                "ck-synapse-opctl",
                "ck-synapse-worker-llama",
                "ck-synapse-worker-mlx",
                "ck-synapse-worker-ane",
                "ck-synapse-worker-decode",
            ]
        );
        assert_eq!(
            component_binaries_for_target(Component::Synapse, AlphaTarget::LinuxX64),
            ["ck-synapse", "ck-synapse-opctl", "ck-synapse-worker-llama"]
        );
    }

    #[test]
    fn sidecar_parser_accepts_the_published_shasum_shape() {
        let digest = "a".repeat(64);
        assert_eq!(
            parse_sidecar(
                &format!("{digest} *ck-subc-linux-x64.zip\n"),
                "ck-subc-linux-x64.zip"
            )
            .expect("valid sidecar"),
            digest
        );
    }

    fn release(tag: &str, created_at: &str, draft: bool, prerelease: bool) -> ReleaseManifest {
        ReleaseManifest {
            tag_name: tag.to_string(),
            assets: vec![
                ReleaseAsset {
                    name: "ck-mc-linux-x64.zip".to_string(),
                },
                ReleaseAsset {
                    name: "ck-mc-linux-x64.zip.sha256".to_string(),
                },
            ],
            draft,
            prerelease,
            created_at: created_at.to_string(),
        }
    }

    fn source_with_manifest(
        component: Component,
        manifest: ReleaseManifest,
    ) -> ReleaseArtifactSource {
        ReleaseArtifactSource {
            target: AlphaTarget::LinuxX64,
            api_base: "https://api.example.test".to_string(),
            release_bases: BTreeMap::new(),
            manifests: BTreeMap::from([(component, manifest)]),
        }
    }

    #[test]
    fn newest_matching_release_uses_created_at_not_tag_order() {
        let newest = newest_matching_release(
            [
                release("ck-mc-alpha.ffffffff", "2026-01-01T00:00:00Z", false, true),
                release("ck-mc-alpha.00000000", "2026-02-01T00:00:00Z", false, true),
                release("v0.41.1", "2026-03-01T00:00:00Z", false, false),
            ],
            "ck-mc-",
        )
        .expect("matching release");
        assert_eq!(newest.tag_name, "ck-mc-alpha.00000000");
    }

    #[test]
    fn prerelease_qualifies_for_mc_resolution() {
        let selected = newest_matching_release(
            [release(
                "ck-mc-alpha.1234abcd",
                "2026-02-01T00:00:00Z",
                false,
                true,
            )],
            "ck-mc-",
        )
        .expect("prerelease qualifies");
        assert_eq!(selected.tag_name, "ck-mc-alpha.1234abcd");
        assert!(selected.prerelease);
    }

    #[test]
    fn draft_matching_release_is_excluded() {
        let selected = newest_matching_release(
            [
                release("ck-mc-alpha.11111111", "2026-01-01T00:00:00Z", false, true),
                release("ck-mc-alpha.22222222", "2026-02-01T00:00:00Z", true, true),
            ],
            "ck-mc-",
        )
        .expect("published matching release");
        assert_eq!(selected.tag_name, "ck-mc-alpha.11111111");
    }

    #[test]
    fn no_matching_release_uses_not_yet_published_arm() {
        let manifest = newest_matching_release(
            [release("v0.41.1", "2026-03-01T00:00:00Z", false, false)],
            "ck-mc-",
        )
        .unwrap_or_else(|| ReleaseManifest {
            tag_name: "ck-mc-*".to_string(),
            assets: Vec::new(),
            draft: false,
            prerelease: false,
            created_at: String::new(),
        });
        let mut source = source_with_manifest(Component::Mc, manifest);
        assert_eq!(
            source
                .release_availability(Component::Mc)
                .expect("availability"),
            ReleaseAvailability::NotYetPublished {
                release_tag: "ck-mc-*".to_string(),
                missing_asset: "ck-mc-linux-x64.zip".to_string(),
            }
        );
    }

    #[test]
    fn only_mc_uses_tag_pattern_resolution() {
        let source = source_with_manifest(
            Component::Mc,
            release("ck-mc-alpha.1234abcd", "2026-01-01T00:00:00Z", false, true),
        );
        assert!(matches!(
            Component::Mc.release_resolution_strategy(),
            ReleaseResolutionStrategy::TagPrefix("ck-mc-")
        ));
        assert_eq!(
            Component::Aft.release_resolution_strategy(),
            ReleaseResolutionStrategy::Latest
        );
        assert_eq!(
            source.manifest_url(Component::Aft),
            "https://api.example.test/repos/cortexkit/aft/releases/latest"
        );
        assert_eq!(
            source.manifest_url(Component::Mc),
            "https://api.example.test/repos/cortexkit/magic-context/releases?per_page=100"
        );
    }
}
