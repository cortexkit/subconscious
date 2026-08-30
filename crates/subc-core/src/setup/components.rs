use std::{
    fs,
    path::Path,
    process::{self, Command},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::{config, inventory::Inventory, model::Component};

pub trait ArtifactSource {
    fn install(&mut self, binary: &str, destination: &Path) -> Result<String, String>;
}

/// Downloads only convention-derived archives and verifies each archive against
/// its matching sidecar before extracting the binary into the managed home.
pub struct ReleaseArtifactSource {
    target: &'static str,
    subc_base_url: String,
    aft_base_url: String,
}

impl ReleaseArtifactSource {
    pub fn current() -> Self {
        Self {
            target: if cfg!(target_os = "macos") {
                "darwin-arm64"
            } else if cfg!(windows) {
                "windows-x64"
            } else {
                "linux-x64"
            },
            subc_base_url: std::env::var("CK_RELEASE_BASE_URL").unwrap_or_else(|_| {
                "https://github.com/cortexkit/subconscious/releases/latest/download".to_string()
            }),
            aft_base_url: std::env::var("CK_AFT_RELEASE_BASE_URL").unwrap_or_else(|_| {
                "https://github.com/cortexkit/aft/releases/latest/download".to_string()
            }),
        }
    }

    fn release_base(&self, binary: &str) -> &str {
        if binary == "ck-aft" {
            &self.aft_base_url
        } else {
            &self.subc_base_url
        }
    }
}

impl ArtifactSource for ReleaseArtifactSource {
    fn install(&mut self, binary: &str, destination: &Path) -> Result<String, String> {
        let (os, arch) = self.target.split_once('-').expect("fixed alpha target");
        let binary_name = platform_binary(binary);
        let archive_name = format!("{binary}-{os}-{arch}.zip");
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
        let base = self.release_base(binary).trim_end_matches('/');
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
}

pub fn component_binaries(component: Component) -> &'static [&'static str] {
    match component {
        Component::Core => &["ck-subc", "ck-subc-mcp"],
        Component::Aft => &["ck-aft"],
        Component::Mc => &[],
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
        let digest = source.install(binary, &destination)?;
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
    inventory: &mut Inventory,
) -> Result<Option<config::ConfigChange>, String> {
    let change =
        config::plan_component(config_path, component, binary_home).map_err(|conflict| {
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
) -> Result<bool, String> {
    match config::plan_component(config_path, component, binary_home) {
        Ok(None) => Ok(true),
        Ok(Some(_)) => Ok(false),
        Err(conflict) => Err(format!("configuration conflict at {}", conflict.key)),
    }
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
        fn install(&mut self, binary: &str, destination: &Path) -> Result<String, String> {
            fs::write(destination, binary).map_err(|error| error.to_string())?;
            digest_file(destination)
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
}
