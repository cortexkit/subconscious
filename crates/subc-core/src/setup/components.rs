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

    /// What `binary` of `component` must prove on its `--version` line.
    fn acceptance(&mut self, component: Component, binary: &str) -> Result<Acceptance, String>;

    /// Confirm the placed binary meets `acceptance`. The default executes
    /// `<destination> --version`, which is what every production source relies
    /// on; a test source may answer from the bytes it wrote instead, because a
    /// fake binary is not executable on every platform.
    fn verify(&mut self, destination: &Path, acceptance: &Acceptance) -> Result<(), String> {
        verify_acceptance(destination, acceptance)
    }
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
            let download_result = download(&url, &temporary);
            // GitHub answers `releases/latest` with 404 when a repository has
            // no published (non-draft) release. That is the owner-has-not-
            // published-yet state the temporal outcome exists for, not a
            // resolution failure: an empty manifest lets availability report
            // exactly which asset is missing under a `latest` that does not
            // exist yet.
            if let Err(error) = &download_result {
                if matches!(
                    component.release_resolution_strategy(),
                    ReleaseResolutionStrategy::Latest
                ) && http_status_from_download_error(error) == Some(404)
                {
                    let _ = fs::remove_file(&temporary);
                    self.manifests.insert(
                        component,
                        ReleaseManifest {
                            tag_name: "latest".to_string(),
                            assets: Vec::new(),
                            draft: false,
                            prerelease: false,
                            created_at: String::new(),
                        },
                    );
                    return Ok(self
                        .manifests
                        .get(&component)
                        .expect("manifest inserted above"));
                }
            }
            download_result
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

    fn acceptance(&mut self, component: Component, binary: &str) -> Result<Acceptance, String> {
        let tag = self.manifest(component)?.tag_name.trim().to_string();
        acceptance_for(component, binary, &tag)
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
        // The managed name is the daemon-placed name (`ck-aft`), not the
        // crate's own binary name: the spec inventory, `ck upgrade`, and the
        // release inventory gate all key on it, so setup must too or an
        // installed aft is never upgradable.
        (Component::Aft, _) => &["ck-aft"],
        (Component::Mc, AlphaTarget::DarwinArm64 | AlphaTarget::LinuxX64) => &["ck-mc"],
        (Component::Mc, AlphaTarget::WindowsX64) => &[],
        (Component::Insula, _) => &["ck-insula"],
        (Component::Claustrum, _) => &["ck-claustrum", "ck-auth"],
        // ck-synapse-worker-mlx is deliberately absent: it is synapse's frozen
        // reference engine, not a serving lane (production Metal embedding runs
        // in-process in ck-synapse), and its metallib can only load from beside
        // the executable, which the one-binary-per-archive contract cannot carry.
        // ck-synapse-worker-ane-swift is the CoreML executable the ane launcher
        // resolves as its sibling, so it ships as its own named asset.
        (Component::Synapse, AlphaTarget::DarwinArm64) => &[
            "ck-synapse",
            "ck-synapse-opctl",
            "ck-synapse-worker-llama",
            "ck-synapse-worker-ane",
            "ck-synapse-worker-ane-swift",
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
        // Acceptance runs between placement and the inventory record. If it
        // refuses, the placed file must not survive: it is owned by nobody, so
        // the next `ck setup` would refuse it as a foreign binary at the
        // managed destination, and the operator could never re-run setup
        // without hand-deleting what setup itself left behind.
        let accepted = source
            .acceptance(component, binary)
            .and_then(|acceptance| source.verify(&destination, &acceptance));
        if let Err(error) = accepted {
            match fs::remove_file(&destination) {
                Ok(()) => return Err(error),
                Err(cleanup) => {
                    return Err(format!(
                    "{error}; additionally could not remove the unaccepted binary at {}: {cleanup}",
                    destination.display()
                ))
                }
            }
        }
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

/// What a placed binary can prove about the release it came from, read off
/// its `--version` line. The archive's sidecar already proves the bytes are
/// the release's; this is the second, independent check that the release
/// itself carries the binary it claims to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Acceptance {
    /// The tag names this binary's crate version; `--version` must report it.
    Version(String),
    /// The tag is a build train; `--version` must carry the train id (mc's
    /// `ck-mc <ver> (<full sha>)` where the tag id is that sha's prefix).
    TrainId(String),
    /// A sibling binary in a multi-crate workspace release: the tag names
    /// another crate's version, and this crate's own version appears nowhere
    /// in the release. It must execute and self-report (the run is the
    /// first-exec toll paid before the daemon spawns it), and provenance rests
    /// on the sidecar. Refusing it against the sibling's version would refuse
    /// every correct release; accepting it on the sidecar is what the sidecar
    /// is for.
    RunsAndReports,
}

/// Per-binary acceptance. The core release ships `ck-subc` (the crate the tag
/// names) beside `ck-subc-mcp` (its own crate, its own version); only the
/// first can be held to the tag's version.
fn acceptance_for(component: Component, binary: &str, tag: &str) -> Result<Acceptance, String> {
    match component.release_resolution_strategy() {
        ReleaseResolutionStrategy::TagPrefix(prefix) => {
            let id = tag.strip_prefix(prefix).unwrap_or(tag).trim();
            // `ck-mc-alpha.<sha8>`: the train id is the part after the channel dot.
            let id = id.rsplit('.').next().unwrap_or(id);
            if id.len() < 7 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(format!(
                    "{component} release tag '{tag}' does not end in a build sha train id"
                ));
            }
            Ok(Acceptance::TrainId(id.to_string()))
        }
        ReleaseResolutionStrategy::Latest => {
            if component == Component::Core && binary != "ck-subc" {
                return Ok(Acceptance::RunsAndReports);
            }
            expected_version_from_tag(component, tag).map(Acceptance::Version)
        }
    }
}

/// The version a placed binary must self-report, derived from the resolved
/// release tag. Owners tag in two shapes: bare `v<version>` (aft, insula,
/// claustrum) and workspace-crate `subc-core-v<version>` (core, the release
/// lane's convention for a multi-crate workspace). Both carry the crate
/// version, so both derive it. A train-shaped tag (`ck-mc-alpha.<sha>`) carries
/// no version at all; acceptance for that shape needs the binary to self-report
/// the train sha, which is the owner's contract to provide — until it does, the
/// refusal names the gap instead of claiming the tag is malformed.
fn expected_version_from_tag(component: Component, tag: &str) -> Result<String, String> {
    let version = tag
        .rsplit_once("-v")
        .map(|(_, rest)| rest)
        .unwrap_or(tag)
        .trim_start_matches('v');
    let looks_like_version = !version.is_empty()
        && version != tag
        && version.chars().next().is_some_and(|c| c.is_ascii_digit());
    if !looks_like_version {
        return Err(format!(
            "latest {component} release tag '{tag}' must be v<crate-version> or \
             <crate>-v<crate-version>"
        ));
    }
    Ok(version.to_string())
}

/// Runs the placed binary before configuration and checks its `--version`
/// line against what the release lets it prove. Prevents a release that
/// carries the wrong binary from becoming a supervised module, and pays the
/// first-exec toll on the destination inode before the daemon spawns it.
fn verify_acceptance(path: &Path, acceptance: &Acceptance) -> Result<(), String> {
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
    check_reported(&reported, acceptance).map_err(|reason| {
        format!(
            "refusal: {} --version {reason}: {reported:?}",
            path.display()
        )
    })
}

/// The pure half of acceptance, over the captured `--version` line.
fn check_reported(reported: &str, acceptance: &Acceptance) -> Result<(), String> {
    match acceptance {
        Acceptance::Version(expected) => {
            if reported
                .split_whitespace()
                .all(|token| token.trim_start_matches('v') != expected)
            {
                return Err(format!("did not report release version {expected}"));
            }
        }
        Acceptance::TrainId(id) => {
            if !reported.contains(id.as_str()) {
                return Err(format!("did not report release train id {id}"));
            }
        }
        Acceptance::RunsAndReports => {
            if reported.split_whitespace().count() < 2 {
                return Err("did not self-report a name and version".to_string());
            }
        }
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
                // The status code is the only thing that distinguishes "no
                // such release yet" from a broken host; write it where the
                // error path can read it.
                "--write-out".to_string(),
                "http_status=%{http_code}".to_string(),
                "--output".to_string(),
                destination,
                url.to_string(),
            ],
        )
    };
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("could not download {url}: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Err(format!(
            "release-incomplete: could not download {url}: {stderr} {stdout}"
        ))
    }
}

/// Best-effort HTTP status recovered from a `download` error message. curl's
/// arm stamps `http_status=NNN` explicitly; the PowerShell arm's exception
/// text carries the code in prose (`(404) Not Found`, `status code ... 404`).
fn http_status_from_download_error(error: &str) -> Option<u16> {
    if let Some(rest) = error.split("http_status=").nth(1) {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(code) = digits.parse::<u16>() {
            if code != 0 {
                return Some(code);
            }
        }
    }
    error
        .split(|c: char| !c.is_ascii_digit())
        .filter(|token| token.len() == 3)
        .filter_map(|token| token.parse::<u16>().ok())
        .find(|code| (400..600).contains(code))
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

    /// On unix the fake is a real shell script and the default `--version`
    /// execution runs unchanged, so the install path is exercised end to end.
    /// Windows cannot execute a script named `.exe`, so there the fake answers
    /// from the bytes it wrote; the execution arm is covered by the alpha CI
    /// workflow against real archives.
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

        fn acceptance(
            &mut self,
            _component: Component,
            _binary: &str,
        ) -> Result<Acceptance, String> {
            Ok(Acceptance::Version("1.2.3".to_string()))
        }

        #[cfg(windows)]
        fn verify(&mut self, destination: &Path, acceptance: &Acceptance) -> Result<(), String> {
            let content = fs::read_to_string(destination).map_err(|error| error.to_string())?;
            // The fake writes `echo <name> <version>`; read the line the real
            // binary would print and run the same pure check over it.
            let reported = content
                .lines()
                .find_map(|line| line.strip_prefix("echo "))
                .unwrap_or("");
            check_reported(reported, acceptance)
                .map_err(|reason| format!("fake binary at {} {reason}", destination.display()))
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

    /// A source whose placed bytes never satisfy acceptance: the binary says
    /// 1.2.3, the release says 9.9.9. Stands in for a wrong-release archive.
    struct WrongReleaseSource;

    impl ArtifactSource for WrongReleaseSource {
        fn install(
            &mut self,
            component: Component,
            binary: &str,
            destination: &Path,
        ) -> Result<String, String> {
            FakeSource.install(component, binary, destination)
        }

        fn acceptance(
            &mut self,
            _component: Component,
            _binary: &str,
        ) -> Result<Acceptance, String> {
            Ok(Acceptance::Version("9.9.9".to_string()))
        }

        #[cfg(windows)]
        fn verify(&mut self, destination: &Path, acceptance: &Acceptance) -> Result<(), String> {
            FakeSource.verify(destination, acceptance)
        }
    }

    /// Found on the first macOS operator drive: a refused acceptance left the
    /// placed binary at the managed destination with no inventory row, and the
    /// next `ck setup` refused it as a foreign file. The operator could not
    /// re-run setup without deleting what setup had left. A refusal must leave
    /// the destination exactly as it found it.
    #[test]
    fn refused_acceptance_removes_the_placed_binary_so_setup_can_rerun() {
        let root = fixture_dir("refused-acceptance");
        let binary_home = root.join("bin");
        fs::create_dir_all(&binary_home).expect("binary home");
        let mut inventory =
            Inventory::load(root.join("installer-manifest.json"), "linux-x64").expect("inventory");
        let error = install_component(
            Component::Core,
            &binary_home,
            &mut inventory,
            &mut WrongReleaseSource,
        )
        .expect_err("wrong-release binary must be refused");
        assert!(
            error.contains("9.9.9"),
            "refusal names the expected version: {error}"
        );
        let placed = binary_home.join(platform_binary("ck-subc"));
        assert!(
            !placed.exists(),
            "refused binary must not survive at {}",
            placed.display()
        );
        assert!(inventory.paths_for_kind("managed-binary").is_empty());
        // The re-run is now the ordinary first-install path, not a foreign-file refusal.
        install_component(
            Component::Core,
            &binary_home,
            &mut inventory,
            &mut FakeSource,
        )
        .expect("re-run after a refused acceptance installs cleanly");
        assert!(is_installed(Component::Core, &binary_home, &inventory));
    }

    #[test]
    fn synapse_uses_the_full_declared_platform_asset_sets() {
        assert_eq!(
            component_binaries_for_target(Component::Synapse, AlphaTarget::DarwinArm64),
            [
                "ck-synapse",
                "ck-synapse-opctl",
                "ck-synapse-worker-llama",
                "ck-synapse-worker-ane",
                "ck-synapse-worker-ane-swift",
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

    /// A repository whose only releases are drafts answers `releases/latest`
    /// with 404. The resolver turns that into an empty `latest` manifest, and
    /// availability must then report the owner-has-not-published state naming
    /// the first missing archive — never a resolution error.
    #[test]
    fn latest_404_reports_not_yet_published_for_the_first_archive() {
        let manifest = ReleaseManifest {
            tag_name: "latest".to_string(),
            assets: Vec::new(),
            draft: false,
            prerelease: false,
            created_at: String::new(),
        };
        let mut source = source_with_manifest(Component::Synapse, manifest);
        assert_eq!(
            source
                .release_availability(Component::Synapse)
                .expect("availability"),
            ReleaseAvailability::NotYetPublished {
                release_tag: "latest".to_string(),
                missing_asset: "ck-synapse-linux-x64.zip".to_string(),
            }
        );
    }

    /// Owners publish two version-carrying tag shapes; both must derive the
    /// version a placed binary self-reports. Found on the first macOS operator
    /// drive: core's real tag `subc-core-v0.14.1` was refused as malformed, so
    /// the alpha was never installable by `ck setup` on any OS while the CI
    /// stub served bare `v<version>` tags the code assumed.
    #[test]
    fn expected_version_derives_from_both_owner_tag_shapes() {
        assert_eq!(
            expected_version_from_tag(Component::Core, "subc-core-v0.14.1").as_deref(),
            Ok("0.14.1")
        );
        assert_eq!(
            expected_version_from_tag(Component::Aft, "v0.55.0").as_deref(),
            Ok("0.55.0")
        );
        assert_eq!(
            expected_version_from_tag(Component::Claustrum, "v0.1.0").as_deref(),
            Ok("0.1.0")
        );
    }

    #[test]
    fn expected_version_refuses_versionless_latest_tags_naming_both_shapes() {
        let error = expected_version_from_tag(Component::Aft, "nightly").unwrap_err();
        assert!(error.contains("v<crate-version>"), "{error}");
        assert!(error.contains("<crate>-v<crate-version>"), "{error}");
    }

    /// Fourth finding of the macOS drive: the core release ships `ck-subc-mcp`
    /// (crate 0.1.0) beside `ck-subc` (the crate the tag names, 0.14.1). One
    /// version cannot hold both; the sibling proves it runs and reports, and
    /// provenance rests on the sidecar.
    #[test]
    fn acceptance_is_per_binary_within_a_workspace_release() {
        assert_eq!(
            acceptance_for(Component::Core, "ck-subc", "subc-core-v0.14.1"),
            Ok(Acceptance::Version("0.14.1".to_string()))
        );
        assert_eq!(
            acceptance_for(Component::Core, "ck-subc-mcp", "subc-core-v0.14.1"),
            Ok(Acceptance::RunsAndReports)
        );
        // Single-crate owners: every binary is the tag's crate.
        assert_eq!(
            acceptance_for(Component::Claustrum, "ck-auth", "v0.1.0"),
            Ok(Acceptance::Version("0.1.0".to_string()))
        );
    }

    /// mc's tag is a build train (`ck-mc-alpha.<sha8>`); its `--version`
    /// self-reports the full build sha, of which the tag id is a prefix.
    #[test]
    fn train_tagged_components_are_accepted_on_the_train_id() {
        assert_eq!(
            acceptance_for(Component::Mc, "ck-mc", "ck-mc-alpha.22464bf2"),
            Ok(Acceptance::TrainId("22464bf2".to_string()))
        );
        let error = acceptance_for(Component::Mc, "ck-mc", "ck-mc-alpha.nightly").unwrap_err();
        assert!(error.contains("build sha train id"), "{error}");
    }

    #[test]
    fn reported_line_check_covers_all_three_acceptance_arms() {
        let version = Acceptance::Version("0.14.1".to_string());
        assert!(check_reported("ck-subc 0.14.1\n", &version).is_ok());
        assert!(check_reported("ck-subc v0.14.1\n", &version).is_ok());
        assert!(check_reported("ck-subc 0.13.0\n", &version).is_err());

        let train = Acceptance::TrainId("22464bf2".to_string());
        assert!(check_reported(
            "ck-mc 0.1.0 (22464bf2a1b2c3d4e5f60718293a4b5c6d7e8f90)\n",
            &train
        )
        .is_ok());
        // An unstamped dev build prints the crate version alone: refused.
        let error = check_reported("ck-mc 0.1.0\n", &train).unwrap_err();
        assert!(error.contains("train id 22464bf2"), "{error}");

        assert!(check_reported("ck-subc-mcp 0.1.0\n", &Acceptance::RunsAndReports).is_ok());
        assert!(check_reported("\n", &Acceptance::RunsAndReports).is_err());
    }

    #[test]
    fn download_error_status_is_recovered_from_both_download_arms() {
        // curl arm: explicit stamp wins even when prose carries other digits.
        assert_eq!(
            http_status_from_download_error(
                "release-incomplete: could not download https://x/releases/latest: curl: (22) The requested URL returned error: 404 http_status=404"
            ),
            Some(404)
        );
        // PowerShell arm: code only in prose.
        assert_eq!(
            http_status_from_download_error(
                "release-incomplete: could not download https://x: The remote server returned an error: (404) Not Found."
            ),
            Some(404)
        );
        // A curl transport error stamps 000 and carries no HTTP code: not a 404.
        assert_eq!(
            http_status_from_download_error(
                "release-incomplete: could not download https://x: curl: (6) Could not resolve host http_status=000"
            ),
            None
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
