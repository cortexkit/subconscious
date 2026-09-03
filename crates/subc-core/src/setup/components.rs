use std::{
    fs,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::{
    config,
    inventory::Inventory,
    model::{AlphaTarget, Component, ReleaseAvailability},
    release_index::{self, IndexAsset, IndexRefusal, ReleaseIndex},
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

/// Downloads archives named by the signed release index and verifies each
/// archive against the index digest before extracting the binary into the
/// managed home. The sidecar files on GitHub are not fetched: the index is
/// the digest source.
pub struct ReleaseArtifactSource {
    target: AlphaTarget,
    index_url: String,
    index: Option<Result<ReleaseIndex, IndexRefusal>>,
}

impl ReleaseArtifactSource {
    pub fn current() -> Self {
        Self {
            target: host_alpha_target(),
            index_url: release_index::index_url(),
            index: None,
        }
    }

    #[cfg(test)]
    pub fn from_index(index: ReleaseIndex, target: AlphaTarget) -> Self {
        Self {
            target,
            index_url: String::new(),
            index: Some(Ok(index)),
        }
    }

    /// Fetch the signed index once. A failure is about the document, not a
    /// single component, so setup must not plan any installation from it.
    pub fn ensure_index(&mut self) -> Result<(), String> {
        self.loaded_index().map(|_| ())
    }

    pub fn release_availability(
        &mut self,
        component: Component,
    ) -> Result<ReleaseAvailability, String> {
        let target = self.target;
        let needed = component_binaries_for_target(component, target);
        let index = self.loaded_index()?;
        let Some(entry) = index.components.get(component.label()) else {
            let missing_asset = needed
                .first()
                .map(|binary| format!("{}-{}.zip", binary, target.label()))
                .unwrap_or_else(|| component.label().to_string());
            return Ok(ReleaseAvailability::NotYetPublished {
                release_tag: "no published release".to_string(),
                missing_asset,
            });
        };
        let target_assets = entry.assets.get(target.label());
        let missing_asset = needed.iter().find_map(|binary| {
            if target_assets.is_some_and(|assets| assets.contains_key(*binary)) {
                None
            } else {
                Some(format!("{}-{}.zip", binary, target.label()))
            }
        });
        Ok(match missing_asset {
            Some(missing_asset) => ReleaseAvailability::NotYetPublished {
                release_tag: entry.release.clone(),
                missing_asset,
            },
            None => ReleaseAvailability::Available,
        })
    }

    fn loaded_index(&mut self) -> Result<&ReleaseIndex, String> {
        if self.index.is_none() {
            self.index = Some(release_index::fetch_index(
                &self.index_url,
                release_index::INSTALL_INDEX_DEADLINE,
            ));
        }
        match self.index.as_ref() {
            Some(Ok(index)) => Ok(index),
            Some(Err(refusal)) => Err(refusal.to_string()),
            None => unreachable!("index is inserted before this match"),
        }
    }

    fn lookup_asset(&mut self, component: Component, binary: &str) -> Result<IndexAsset, String> {
        let target = self.target;
        let index = self.loaded_index()?;
        index
            .components
            .get(component.label())
            .and_then(|entry| entry.assets.get(target.label()))
            .and_then(|assets| assets.get(binary))
            .cloned()
            .ok_or_else(|| {
                format!(
                    "release-incomplete: no {binary}-{} asset for {component}",
                    target.label()
                )
            })
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
        let asset = self.lookup_asset(component, binary)?;
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
        release_index::download(&asset.url, &archive)?;
        let expected = asset.sha256.to_ascii_lowercase();
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
        let asset = self.lookup_asset(component, binary)?;
        Ok(match asset.reports {
            Some(reports) => Acceptance::Reports(reports),
            None => Acceptance::RunsAndReports,
        })
    }
}

pub fn component_binaries(component: Component) -> &'static [&'static str] {
    component_binaries_for_target(component, host_alpha_target())
}

/// The binary the daemon spawns for a module. Target-independent by name:
/// what varies per target is the sidecar set beside it, never the program.
/// Core is not a module the daemon spawns, so it has no program here.
///
/// This is the name the configuration writer records, so it must be the
/// first managed binary on every target that carries the component at all
/// — `module_program_leads_every_target_set` pins that against the
/// per-target table. Two tables encoding one fact drift unless a test binds
/// them; that test is what makes a second table admissible.
pub fn module_program(component: Component) -> Option<&'static str> {
    match component {
        Component::Core => None,
        Component::Aft => Some("ck-aft"),
        Component::Mc => Some("ck-mc"),
        Component::Insula => Some("ck-insula"),
        Component::Claustrum => Some("ck-claustrum"),
        Component::Synapse => Some("ck-synapse"),
    }
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
        // The ownership record must be durable the moment the binary it
        // describes is. Deferring the save to the end of the whole plan meant
        // a refusal anywhere later left every already-accepted binary on disk
        // with its record lost — and the next `ck setup` refused them as
        // foreign files at managed destinations. A binary placed, accepted,
        // and recorded is a completed mutation regardless of what the plan
        // does next.
        inventory.save().map_err(|error| {
            format!(
                "placed and accepted {} but could not record ownership: {error}",
                destination.display()
            )
        })?;
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

/// What a placed binary can prove about the release it came from, read off
/// its `--version` line. The index digest already proves the bytes are the
/// release's; this is the second, independent check that the binary reports
/// what the index said it would.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Acceptance {
    /// `--version` output must contain this substring, copied from the
    /// index asset's `reports` field.
    Reports(String),
    /// The index did not name a substring. The binary must execute and print
    /// a name and a version; provenance rests on the verified sha256.
    RunsAndReports,
}

/// Runs the placed binary before configuration and checks its `--version`
/// line against what the release lets it prove. Prevents a release that
/// carries the wrong binary from becoming a supervised module, and pays the
/// first-exec toll on the destination inode before the daemon spawns it.
fn verify_acceptance(path: &Path, acceptance: &Acceptance) -> Result<(), String> {
    let output = run_version_tolerating_text_busy(path)
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

/// Executes `<path> --version`, retrying briefly on ETXTBSY. On Linux a
/// `fork` in another thread of this process inherits every open descriptor,
/// including the write end of a binary that a sibling thread is still
/// finishing; until that child execs (which closes it), the kernel refuses
/// to execute the file as "text busy". The window is microseconds but the
/// error is real, and the fix belongs here rather than in a test because
/// the same race exists for any multi-threaded caller of this acceptance.
fn run_version_tolerating_text_busy(path: &Path) -> std::io::Result<std::process::Output> {
    const TEXT_BUSY: i32 = 26;
    let mut attempts = 0;
    loop {
        match Command::new(path).arg("--version").output() {
            Err(error) if error.raw_os_error() == Some(TEXT_BUSY) && attempts < 20 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            result => return result,
        }
    }
}

/// The pure half of acceptance, over the captured `--version` line.
fn check_reported(reported: &str, acceptance: &Acceptance) -> Result<(), String> {
    match acceptance {
        Acceptance::Reports(expected) => {
            if !reported.contains(expected.as_str()) {
                return Err(format!("did not report {expected}"));
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

pub fn digest_file(path: &Path) -> Result<String, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("could not hash {}: {error}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn host_alpha_target() -> AlphaTarget {
    if cfg!(target_os = "macos") {
        AlphaTarget::DarwinArm64
    } else if cfg!(windows) {
        AlphaTarget::WindowsX64
    } else {
        AlphaTarget::LinuxX64
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
            Ok(Acceptance::Reports("1.2.3".to_string()))
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

    /// The program table and the per-target binary table encode one fact
    /// twice; this is the binding that keeps them from drifting. Every
    /// target that carries a module at all must list its program first, and
    /// core — which the daemon does not spawn — has no program.
    #[test]
    fn module_program_leads_every_target_set() {
        for component in Component::ALL {
            for target in [
                AlphaTarget::DarwinArm64,
                AlphaTarget::LinuxX64,
                AlphaTarget::WindowsX64,
            ] {
                let set = component_binaries_for_target(component, target);
                match module_program(component) {
                    None => assert_eq!(component, Component::Core, "{component} needs a program"),
                    Some(program) => {
                        if let Some(first) = set.first() {
                            assert_eq!(
                                *first, program,
                                "{component} on {target:?}: the daemon-spawned program must lead the set"
                            );
                        }
                    }
                }
            }
        }
        // The empty set is a real state (mc on Windows), and the program
        // must still resolve there: the config is target-independent.
        assert!(component_binaries_for_target(Component::Mc, AlphaTarget::WindowsX64).is_empty());
        assert_eq!(module_program(Component::Mc), Some("ck-mc"));
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
            Ok(Acceptance::Reports("9.9.9".to_string()))
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

    /// Accepts the first binary of a component and refuses the second. Stands
    /// in for the real macOS drive shape: `ck-subc` accepted, `ck-subc-mcp`
    /// refused, then a re-run.
    struct SecondBinaryRefusesSource;

    impl ArtifactSource for SecondBinaryRefusesSource {
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
            binary: &str,
        ) -> Result<Acceptance, String> {
            Ok(Acceptance::Reports(
                if binary == "ck-subc" {
                    "1.2.3"
                } else {
                    "9.9.9"
                }
                .to_string(),
            ))
        }

        #[cfg(windows)]
        fn verify(&mut self, destination: &Path, acceptance: &Acceptance) -> Result<(), String> {
            FakeSource.verify(destination, acceptance)
        }
    }

    /// Sixth finding of the macOS drive, the parent of the third: ownership
    /// was saved once at the end of the whole plan, so a refusal on the
    /// second binary lost the record for the first, already-accepted one —
    /// on disk, unowned, refused as foreign on the re-run. The record must
    /// survive on disk the moment the binary does; proven by reloading the
    /// inventory from disk between the refused run and the re-run.
    #[test]
    fn accepted_binaries_stay_owned_on_disk_when_a_later_one_is_refused() {
        let root = fixture_dir("partial-refusal");
        let binary_home = root.join("bin");
        fs::create_dir_all(&binary_home).expect("binary home");
        let manifest_path = root.join("installer-manifest.json");
        {
            let mut inventory =
                Inventory::load(manifest_path.clone(), "linux-x64").expect("inventory");
            install_component(
                Component::Core,
                &binary_home,
                &mut inventory,
                &mut SecondBinaryRefusesSource,
            )
            .expect_err("second binary must be refused");
            // Deliberately NOT saving here: the record must already be on disk.
        }
        assert!(binary_home.join(platform_binary("ck-subc")).is_file());
        assert!(!binary_home.join(platform_binary("ck-subc-mcp")).exists());

        let mut reloaded = Inventory::load(manifest_path, "linux-x64").expect("reload from disk");
        assert!(
            reloaded.owns_path(
                "managed-binary",
                &binary_home.join(platform_binary("ck-subc"))
            ),
            "the accepted binary's ownership record must have survived on disk"
        );
        // Re-run with a source that accepts both: first is a no-op, second installs.
        install_component(
            Component::Core,
            &binary_home,
            &mut reloaded,
            &mut FakeSource,
        )
        .expect("re-run after a partial refusal installs the rest cleanly");
        assert!(is_installed(Component::Core, &binary_home, &reloaded));
        assert_eq!(reloaded.paths_for_kind("managed-binary").len(), 2);
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

    fn asset(reports: Option<&str>) -> super::super::release_index::IndexAsset {
        super::super::release_index::IndexAsset {
            url: "http://127.0.0.1/archive.zip".to_string(),
            sha256: "ab".repeat(32),
            reports: reports.map(str::to_string),
        }
    }

    fn index_with_core_linux() -> ReleaseIndex {
        let mut binaries = std::collections::BTreeMap::new();
        binaries.insert("ck-subc".to_string(), asset(Some("0.16.0")));
        binaries.insert("ck-subc-mcp".to_string(), asset(None));
        let mut targets = std::collections::BTreeMap::new();
        targets.insert("linux-x64".to_string(), binaries);
        let mut components = std::collections::BTreeMap::new();
        components.insert(
            "core".to_string(),
            super::super::release_index::IndexComponent {
                release: "subc-core-v0.16.0".to_string(),
                version: Some("0.16.0".to_string()),
                assets: targets,
            },
        );
        ReleaseIndex {
            schema: 1,
            channel: "alpha".to_string(),
            generated_at_ms: 1_788_425_000_000,
            components,
        }
    }

    #[test]
    fn absent_component_is_not_yet_published() {
        let mut source =
            ReleaseArtifactSource::from_index(index_with_core_linux(), AlphaTarget::LinuxX64);
        assert_eq!(
            source
                .release_availability(Component::Aft)
                .expect("availability"),
            ReleaseAvailability::NotYetPublished {
                release_tag: "no published release".to_string(),
                missing_asset: "ck-aft-linux-x64.zip".to_string(),
            }
        );
    }

    #[test]
    fn present_component_missing_host_target_names_the_missing_asset() {
        let mut source =
            ReleaseArtifactSource::from_index(index_with_core_linux(), AlphaTarget::DarwinArm64);
        assert_eq!(
            source
                .release_availability(Component::Core)
                .expect("availability"),
            ReleaseAvailability::NotYetPublished {
                release_tag: "subc-core-v0.16.0".to_string(),
                missing_asset: "ck-subc-darwin-arm64.zip".to_string(),
            }
        );
    }

    #[test]
    fn complete_host_assets_are_available() {
        let mut source =
            ReleaseArtifactSource::from_index(index_with_core_linux(), AlphaTarget::LinuxX64);
        assert_eq!(
            source
                .release_availability(Component::Core)
                .expect("availability"),
            ReleaseAvailability::Available
        );
    }

    #[test]
    fn reports_some_acceptance_passes_and_fails_on_the_version_line() {
        let mut source =
            ReleaseArtifactSource::from_index(index_with_core_linux(), AlphaTarget::LinuxX64);
        assert_eq!(
            source
                .acceptance(Component::Core, "ck-subc")
                .expect("acceptance"),
            Acceptance::Reports("0.16.0".to_string())
        );
        let reports = Acceptance::Reports("0.16.0".to_string());
        assert!(check_reported("ck-subc 0.16.0\n", &reports).is_ok());
        assert!(check_reported("ck-subc v0.16.0\n", &reports).is_ok());
        assert!(check_reported("ck-subc 0.15.0\n", &reports).is_err());
    }

    #[test]
    fn reports_none_requires_a_name_and_version() {
        let mut source =
            ReleaseArtifactSource::from_index(index_with_core_linux(), AlphaTarget::LinuxX64);
        assert_eq!(
            source
                .acceptance(Component::Core, "ck-subc-mcp")
                .expect("acceptance"),
            Acceptance::RunsAndReports
        );
        assert!(check_reported("ck-subc-mcp 0.1.0\n", &Acceptance::RunsAndReports).is_ok());
        assert!(check_reported("\n", &Acceptance::RunsAndReports).is_err());
    }

    #[test]
    fn available_core_index_plans_install_and_offers_extras() {
        let mut source =
            ReleaseArtifactSource::from_index(index_with_core_linux(), AlphaTarget::LinuxX64);
        assert_eq!(
            source.release_availability(Component::Core).unwrap(),
            ReleaseAvailability::Available
        );
        let mut observed = super::super::model::SetupObserved::unconfigured_current_host();
        observed.platform =
            super::super::model::PlatformObservation::Supported(AlphaTarget::LinuxX64);
        observed
            .releases
            .insert(Component::Core, ReleaseAvailability::Available);
        let plan = super::super::planner::plan_setup(
            &observed,
            &super::super::model::SetupRequest::install(Vec::new()),
        );
        assert!(plan.operations.iter().any(|operation| matches!(
            operation,
            super::super::model::SetupOperation::InstallComponent {
                component: Component::Core
            }
        )));
        assert!(plan.operations.iter().any(|operation| matches!(
            operation,
            super::super::model::SetupOperation::OfferOptionalComponents
        )));
    }
}
