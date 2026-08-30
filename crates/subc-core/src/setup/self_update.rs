use std::{fmt, path::Path};

#[cfg(not(any(unix, windows)))]
use std::path::PathBuf;
#[cfg(windows)]
use std::{env, path::PathBuf};

use serde_json::Map;

use super::{inventory::Inventory, upgrade_assets::sha256_file};

/// Evidence emitted only after the replacement destination and its ownership
/// record agree. A failed manifest write remains a refusal because later
/// uninstall must never mistake the updater's replacement for a user edit.
#[derive(Debug)]
pub(crate) struct SelfUpdateEvidence {
    detail: String,
}

impl fmt::Display for SelfUpdateEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

/// Replaces an inventory-owned `ck` only after the upgrade asset layer has
/// downloaded and checked the convention-derived archive and its sidecar.
///
/// The candidate is intentionally accepted only from that layer. Keeping the
/// platform replacement here prevents a generic binary replacement from
/// deleting a Windows rollback file or bypassing manifest reconciliation.
pub(crate) fn replace_verified_candidate(
    destination: &Path,
    candidate: &Path,
    inventory: &mut Inventory,
) -> Result<SelfUpdateEvidence, String> {
    if !candidate.is_file() {
        return Err(format!(
            "refusal: verified ck replacement candidate is missing: {}",
            candidate.display()
        ));
    }

    let owned_kinds = ["managed-binary", "binary-placement"]
        .into_iter()
        .filter(|kind| inventory.owns_path(kind, destination))
        .collect::<Vec<_>>();
    if owned_kinds.is_empty() {
        return Err(format!(
            "refusal: installer-manifest.json does not own ck destination {}; refusing self-update",
            destination.display()
        ));
    }

    #[cfg(unix)]
    let (replacement_detail, rollback_path) =
        super::self_update_unix::replace_verified_candidate(destination, candidate)?;
    #[cfg(windows)]
    let (replacement_detail, rollback_path) =
        super::self_update_windows::replace_verified_candidate(destination, candidate, inventory)?;
    #[cfg(not(any(unix, windows)))]
    let (replacement_detail, rollback_path): (String, Option<PathBuf>) = {
        return Err("refusal: self-update is unsupported on this platform".to_string());
    };

    if let Some(rollback_path) = rollback_path {
        let mut fields = Map::new();
        fields.insert(
            "replaces".to_string(),
            serde_json::Value::String(destination.to_string_lossy().into_owned()),
        );
        inventory.record("self-update-rollback", &rollback_path, fields);
    }

    let digest = sha256_file(destination).map_err(|error| {
        format!(
            "refusal: self-update installed {} but could not read it for installer-manifest.json reconciliation: {error}",
            destination.display()
        )
    })?;
    for kind in owned_kinds {
        inventory
            .update_owned_string(kind, destination, "sha256", digest.clone())
            .map_err(|error| {
                format!(
                    "refusal: self-update installed {} but could not reconcile installer-manifest.json: {error}",
                    destination.display()
                )
            })?;
    }
    inventory.save().map_err(|error| {
        format!(
            "refusal: self-update installed {} but could not save installer-manifest.json: {error}",
            destination.display()
        )
    })?;

    Ok(SelfUpdateEvidence {
        detail: format!("{replacement_detail}; installer-manifest.json SHA-256 reconciled"),
    })
}

#[cfg(windows)]
pub(crate) fn cleanup_replaced_windows_ck(executable: &Path) -> Result<(), String> {
    let manifest = current_windows_manifest()?;
    let mut inventory = Inventory::load(&manifest, "windows-x64")?;
    super::self_update_windows::cleanup_owned_previous(executable, &mut inventory)
}

#[cfg(windows)]
fn current_windows_manifest() -> Result<PathBuf, String> {
    let data_home = env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "LOCALAPPDATA is unavailable for Windows self-update cleanup".to_string())?;
    Ok(data_home.join("cortexkit").join("installer-manifest.json"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::{Map, Value};

    use super::*;
    use subc_core::test_support::TestTempDir;

    fn fixture_dir(name: &str) -> TestTempDir {
        TestTempDir::new(name)
    }

    #[test]
    fn refuses_to_replace_an_unowned_destination() {
        let root = fixture_dir("unowned");
        let destination = root.join(if cfg!(windows) { "ck.exe" } else { "ck" });
        let candidate = root.join("candidate");
        let manifest = root.join("installer-manifest.json");
        fs::write(&destination, "user-owned").expect("destination");
        fs::write(&candidate, "replacement").expect("candidate");
        let mut inventory = Inventory::load(&manifest, "linux-x64").expect("inventory");

        let error = replace_verified_candidate(&destination, &candidate, &mut inventory)
            .expect_err("unowned destination must refuse");

        assert!(error.contains("installer-manifest.json does not own"));
        assert_eq!(
            fs::read_to_string(&destination).expect("destination bytes"),
            "user-owned"
        );
        assert!(!destination.with_extension("exe.old").exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn replacement_updates_only_the_owned_destination_digest() {
        let root = fixture_dir("manifest");
        let destination = root.join(if cfg!(windows) { "ck.exe" } else { "ck" });
        let candidate = root.join("candidate");
        let unrelated = root.join("user-notes.txt");
        let manifest = root.join("installer-manifest.json");
        fs::write(&destination, "previous").expect("destination");
        fs::write(&candidate, "replacement").expect("candidate");
        fs::write(&unrelated, "keep me").expect("unrelated file");

        let mut inventory = Inventory::load(&manifest, "linux-x64").expect("inventory");
        let mut fields = Map::new();
        fields.insert("sha256".to_string(), Value::String("previous".to_string()));
        inventory.record("binary-placement", &destination, fields);
        inventory.save().expect("save inventory");

        let evidence = replace_verified_candidate(&destination, &candidate, &mut inventory)
            .expect("owned replacement");
        assert!(evidence.to_string().contains("SHA-256 reconciled"));
        assert_eq!(
            fs::read_to_string(&destination).expect("destination bytes"),
            "replacement"
        );
        assert_eq!(
            fs::read_to_string(&unrelated).expect("unrelated bytes"),
            "keep me"
        );

        let inventory = Inventory::load(&manifest, "linux-x64").expect("reloaded inventory");
        let replacement_digest = sha256_file(&destination).expect("replacement digest");
        assert_eq!(
            inventory
                .entry_for_path("binary-placement", &destination)
                .and_then(|entry| entry.get("sha256"))
                .and_then(Value::as_str),
            Some(replacement_digest.as_str())
        );
    }
}
