use super::{file_modes, install_journal::directory, inventory::Inventory, release_index};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Seek},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub(super) const ASSET: &str = "ck-plexus-catalog";
const KIND: &str = "plexus-catalog";

pub(super) fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("plexus").join("catalog")
}

fn owned_digest(path: &Path, inventory: &Inventory) -> Result<Option<String>, String> {
    let parent = path.parent().expect("catalog has a parent");
    match fs::symlink_metadata(parent) {
        Ok(meta) if !meta.is_dir() || super::install_journal::is_reparse_point(&meta) => {
            return Err(format!(
                "refusal: {} is not a plain directory",
                parent.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    let tree = directory::digest(path)?;
    let owned = inventory.entry_for_path(KIND, path);
    if tree.is_some()
        && tree.as_deref()
            != owned
                .and_then(|row| row.get("tree_sha256"))
                .and_then(Value::as_str)
    {
        return Err(format!(
            "refusal: {} is not the managed Plexus catalog; move it aside and retry",
            path.display()
        ));
    }
    Ok(tree)
}

pub(super) fn needs_refresh(
    path: &Path,
    inventory: &Inventory,
    sha256: &str,
) -> Result<bool, String> {
    Ok(installed_archive(path, inventory)?.as_deref() != Some(sha256))
}

pub(super) fn installed_archive(
    path: &Path,
    inventory: &Inventory,
) -> Result<Option<String>, String> {
    if owned_digest(path, inventory)?.is_none() {
        return Ok(None);
    }
    Ok(inventory
        .entry_for_path(KIND, path)
        .and_then(|row| row.get("archive_sha256"))
        .and_then(Value::as_str)
        .map(str::to_owned))
}

pub(super) fn ensure(
    path: &Path,
    inventory: &mut Inventory,
    url: &str,
    sha256: &str,
) -> Result<(), String> {
    ensure_with(path, inventory, sha256, |file| {
        release_index::download(url, file)
    })
}

fn ensure_with(
    path: &Path,
    inventory: &mut Inventory,
    sha256: &str,
    download: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    if !needs_refresh(path, inventory, sha256)? {
        return Ok(());
    }
    let before = owned_digest(path, inventory)?;
    let workspace = Scratch::new()?;
    let archive = workspace.0.join("catalog.zip");
    download(&archive)?;
    let mut file = subc_os::open_regular_nofollow(&archive).map_err(|error| error.to_string())?;
    let mut hash = Sha256::new();
    io::copy(&mut file, &mut hash).map_err(|error| error.to_string())?;
    let actual = format!("{:x}", hash.finalize());
    if actual != sha256 {
        return Err(format!(
            "{ASSET} SHA-256 mismatch: expected {sha256}, found {actual}"
        ));
    }
    file.rewind().map_err(|error| error.to_string())?;
    let candidate = workspace.0.join("catalog");
    file_modes::create_private_directory(&candidate)?;
    extract(file, &candidate)?;
    let transaction = inventory
        .transaction()
        .ok_or("Plexus catalog requires an install transaction")?;
    let digest = directory::place(transaction.journal(), path, &candidate, before)?;
    if inventory.owns_path(KIND, path) {
        inventory.update_owned_string(KIND, path, "archive_sha256", sha256.into())?;
        inventory.update_owned_string(KIND, path, "tree_sha256", digest)?;
    } else {
        inventory.record(
            KIND,
            path,
            json!({"archive_sha256": sha256, "tree_sha256": digest})
                .as_object()
                .unwrap()
                .clone(),
        );
    }
    Ok(())
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self, String> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("ck-plexus-catalog-{}-{nonce}", std::process::id()));
        file_modes::create_private_directory(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn extract(file: fs::File, destination: &Path) -> Result<(), String> {
    let invalid = |detail| format!("invalid {ASSET} ZIP: {detail}");
    let mut directory = file
        .try_clone()
        .map_err(|error| invalid(error.to_string()))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|error| invalid(error.to_string()))?;
    super::apply::extension::reject_duplicate_headers(
        &mut directory,
        zip.central_directory_start(),
        zip.len(),
    )
    .map_err(|error| invalid(error.to_string()))?;
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|error| invalid(error.to_string()))?;
        let name =
            std::str::from_utf8(entry.name_raw()).map_err(|error| invalid(error.to_string()))?;
        if !entry.is_file()
            || !matches!(entry.unix_mode().unwrap_or(0) & 0o170000, 0 | 0o100000)
            || !plain_name(name)
        {
            return Err(invalid(format!(
                "expected a plain root .jsonc file, found {name:?}"
            )));
        }
        let target = destination.join(name);
        let mut output =
            fs::File::create_new(&target).map_err(|error| invalid(error.to_string()))?;
        io::copy(&mut entry, &mut output).map_err(|error| invalid(error.to_string()))?;
    }
    Ok(())
}

fn plain_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".jsonc") else {
        return false;
    };
    if stem.is_empty()
        || name
            .chars()
            .any(|c| c.is_control() || "/\\:<>\"|?*".contains(c))
    {
        return false;
    }
    // Reject names that Windows treats as devices even with an extension.
    let first = stem
        .split('.')
        .next()
        .unwrap()
        .trim_end()
        .to_ascii_uppercase();
    !matches!(first.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        && !["COM", "LPT"].into_iter().any(|prefix| {
            matches!(
                first.strip_prefix(prefix),
                Some("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
            )
        })
}

#[cfg(test)]
mod tests;
