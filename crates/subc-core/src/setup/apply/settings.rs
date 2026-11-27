use super::{Inventory, RuntimePlatform};
use crate::setup::install_journal::files;
use std::{
    fs, io,
    path::{Path, PathBuf},
};
use subc_daemon::daemon_config::{self, StorageConfig};

const RECEIPT: &str = "settings-receipt";
const FUSIFORM: &str = concat!(
    r#"{"source":{"mode":"store","url":"https://models.cortexkit.io","keys":[{"kid":"models-1","public_key":"YMEG61WYx6n19Xs44Q6FlEmsbEee_gvDnpl7GOjwz2U"}]}}"#,
    "\n"
);
const MAC_CHROME: &str = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const LINUX_CHROME: &str = "/usr/bin/google-chrome";
const LINUX_CHROME_IMAGE: &str = "/opt/google/chrome/chrome";

pub(super) fn present(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("could not inspect {}: {error}", path.display())),
    }
}

pub(super) fn receipt(path: &Path, inventory: &mut Inventory) {
    inventory.record(RECEIPT, path, serde_json::Map::new());
}

pub(super) fn write(path: &Path, bytes: &[u8], inventory: &mut Inventory) -> Result<bool, String> {
    let created = inventory
        .transaction()
        .ok_or("settings write has no install transaction")?
        .write_create_only(path, bytes, 0o600)?;
    if created {
        receipt(path, inventory);
    }
    Ok(created)
}

pub(super) enum Plan {
    Write,
    Note(String),
    Skip,
}

pub(super) fn plan(
    path: &Path,
    inventory: &Inventory,
    precondition: impl FnOnce() -> Result<Option<String>, String>,
) -> Result<Plan, String> {
    if present(path)? {
        return Ok(if inventory.owns_path(RECEIPT, path) {
            Plan::Skip
        } else {
            Plan::Note(left_as_is(path))
        });
    }
    Ok(match precondition()? {
        Some(note) => Plan::Note(note),
        None => Plan::Write,
    })
}

pub(super) fn left_as_is(path: &Path) -> String {
    format!("{} left as it is", path.display())
}

fn store_path(config: &Path) -> Result<PathBuf, String> {
    let storage = daemon_config::load(config)
        .map_err(|error| error.to_string())?
        .and_then(|config| config.storage)
        .unwrap_or_else(|| StorageConfig::Sqlite {
            data_home: daemon_config::default_data_home(),
        });
    let descriptor = storage.descriptor_for("fusiform");
    descriptor
        .pointer("/backend/path")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| "Fusiform storage descriptor has no SQLite path".into())
}

pub(super) fn fusiform_precondition(config: &Path, path: &Path) -> Result<Option<String>, String> {
    let store = store_path(config)?;
    Ok(present(&store)?.then(|| {
        format!(
            "{} not written: existing store.db at {}",
            path.display(),
            store.display()
        )
    }))
}

pub(super) fn ensure_fusiform(
    config: &Path,
    path: &Path,
    inventory: &mut Inventory,
) -> Result<String, String> {
    if present(path)? {
        return Ok(left_as_is(path));
    }
    if let Some(note) = fusiform_precondition(config, path)? {
        return Ok(note);
    }
    Ok(write_report(
        path,
        write(path, FUSIFORM.as_bytes(), inventory)?,
    ))
}

#[derive(Clone, Copy)]
struct Chrome<'a> {
    mac: &'a Path,
    linux: &'a Path,
    linux_image: &'a Path,
}

impl Chrome<'_> {
    fn std_paths() -> Self {
        Self {
            mac: Path::new(MAC_CHROME),
            linux: Path::new(LINUX_CHROME),
            linux_image: Path::new(LINUX_CHROME_IMAGE),
        }
    }

    fn available(self, platform: RuntimePlatform) -> bool {
        match platform {
            RuntimePlatform::Macos => self.mac.is_file(),
            RuntimePlatform::Linux => self.linux.is_file() && self.linux_image.is_file(),
            RuntimePlatform::Windows => false,
        }
    }

    fn bytes(self, platform: RuntimePlatform) -> Result<Option<String>, String> {
        if !self.available(platform) {
            return Ok(None);
        }
        let (executable, identity) = match platform {
            RuntimePlatform::Macos => (MAC_CHROME, "com.google.Chrome".to_owned()),
            RuntimePlatform::Linux => (
                LINUX_CHROME,
                files::digest(self.linux_image)?.ok_or_else(|| {
                    format!(
                        "{} disappeared while checking Chrome",
                        self.linux_image.display()
                    )
                })?,
            ),
            RuntimePlatform::Windows => return Ok(None),
        };
        Ok(Some(format!("{{\"browser\":{{\"executable_path\":\"{executable}\",\"expected_identity\":\"{identity}\"}}}}\n")))
    }
}

fn chrome_absent() -> String {
    "cerebellum settings not written: Google Chrome not found; the agent browser stays off".into()
}

pub(super) fn cerebellum_precondition(platform: RuntimePlatform) -> Option<String> {
    (!Chrome::std_paths().available(platform)).then(chrome_absent)
}

pub(super) fn ensure_cerebellum(
    platform: RuntimePlatform,
    path: &Path,
    inventory: &mut Inventory,
) -> Result<String, String> {
    if present(path)? {
        return Ok(left_as_is(path));
    }
    let Some(bytes) = Chrome::std_paths().bytes(platform)? else {
        return Ok(chrome_absent());
    };
    Ok(write_report(
        path,
        write(path, bytes.as_bytes(), inventory)?,
    ))
}

fn write_report(path: &Path, created: bool) -> String {
    if created {
        format!("{}: ensured", path.display())
    } else {
        left_as_is(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    #[test]
    fn cerebellum_chrome_settings_require_both_linux_paths_and_pin_actual_image_bytes() {
        let root = ScratchDir::new("cerebellum-chrome-paths");
        let mac = root.join("mac-chrome");
        let linux = root.join("linux-launcher");
        let linux_image = root.join("linux-image");
        let chrome = Chrome {
            mac: &mac,
            linux: &linux,
            linux_image: &linux_image,
        };
        assert_eq!(chrome.bytes(RuntimePlatform::Macos).unwrap(), None);
        assert_eq!(chrome.bytes(RuntimePlatform::Linux).unwrap(), None);
        fs::write(&mac, b"image").unwrap();
        assert_eq!(
            chrome.bytes(RuntimePlatform::Macos).unwrap().unwrap(),
            concat!(
                r#"{"browser":{"executable_path":"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome","expected_identity":"com.google.Chrome"}}"#,
                "\n"
            )
        );
        fs::write(&linux, b"not the image").unwrap();
        assert_eq!(chrome.bytes(RuntimePlatform::Linux).unwrap(), None);
        fs::write(&linux_image, b"abc").unwrap();
        assert_eq!(
            chrome.bytes(RuntimePlatform::Linux).unwrap().unwrap(),
            concat!(
                r#"{"browser":{"executable_path":"/usr/bin/google-chrome","expected_identity":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}}"#,
                "\n"
            )
        );
        assert_eq!(chrome.bytes(RuntimePlatform::Windows).unwrap(), None);
    }
}
