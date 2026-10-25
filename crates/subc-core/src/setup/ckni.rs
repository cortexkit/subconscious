//! `ckni` beside the managed `ck`.
//!
//! `ckni` is the same program as `ck`, run under another name: `ck` picks its
//! plain, never-prompting behaviour from the name it was started under. On
//! Unix the name is a symlink to `ck` in the same folder. Windows symlinks
//! need a privilege ordinary accounts lack, so there `ckni.exe` is a byte copy
//! of `ck.exe`, and every step that changes `ck.exe` rewrites the copy.
//!
//! The installer manifest records the name under the `managed-link` kind,
//! which uninstall already removes. A symlink row carries no `sha256`, because
//! hashing would read `ck` through the link; a copy row carries the copy's
//! `sha256`, so uninstall keeps a copy someone has since changed. Setup and
//! upgrade never replace a `ckni` the manifest doesn't list.

use std::{
    fs, io,
    path::{Path, PathBuf},
    process,
};

use serde_json::{Map, Value};

use super::{
    components::{digest_file, OWNED_BINARY_KINDS},
    file_modes,
    inventory::Inventory,
    managed_replace,
};

/// The installer-manifest kind for `ckni`. Uninstall removes every path
/// recorded under it before it removes binaries.
pub(crate) const CKNI_KIND: &str = "managed-link";

/// How `ckni` reaches `ck`'s bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mechanism {
    /// A relative symlink named `ckni` pointing at `ck`'s file name.
    Symlink,
    /// A byte copy of `ck`, named `ckni.exe` on Windows.
    Copy,
}

impl Mechanism {
    /// The mechanism this host uses.
    pub const CURRENT: Mechanism = if cfg!(windows) {
        Mechanism::Copy
    } else {
        Mechanism::Symlink
    };

    fn label(self) -> &'static str {
        match self {
            Mechanism::Symlink => "symlink",
            Mechanism::Copy => "copy",
        }
    }

    /// What setup's plan says it will do, before the path.
    pub fn plan_verb(self) -> &'static str {
        match self {
            Mechanism::Symlink => "link ckni to ck",
            Mechanism::Copy => "copy ck to ckni",
        }
    }
}

/// What is at the `ckni` path beside a managed `ck`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CkniState {
    /// Nothing is there.
    Missing,
    /// The manifest lists it and it matches `ck`: a link whose target names
    /// `ck`, or a copy whose digest equals `ck`'s.
    Current,
    /// The manifest lists it but it no longer matches `ck`: a copy whose
    /// digest differs from `ck`'s, or a link that points elsewhere.
    Stale,
    /// Something the manifest doesn't list is there. Setup and upgrade
    /// refuse rather than replace it.
    Unowned,
}

/// The `ckni` path beside one managed `ck`, and what is there now.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CkniObservation {
    pub ck: PathBuf,
    pub link: PathBuf,
    pub state: CkniState,
}

/// The `ckni` path beside `ck`: `ckni.exe` when `ck` has the `.exe`
/// extension, `ckni` otherwise.
pub fn link_path(ck: &Path) -> PathBuf {
    let exe = ck
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"));
    ck.with_file_name(if exe { "ckni.exe" } else { "ckni" })
}

/// Whether the installer manifest records `ck` as an owned binary (placed by
/// setup or by the bootstrap installer).
/// `ckni` is only ever put beside a `ck` the manifest owns, so a `ck` run
/// from a build folder or a package manager never gains a sibling.
pub fn ck_is_owned(ck: &Path, inventory: &Inventory) -> bool {
    OWNED_BINARY_KINDS
        .iter()
        .any(|kind| inventory.owns_path(kind, ck))
}

/// Reads what is at the `ckni` path beside `ck` without changing anything.
pub fn observe(ck: &Path, inventory: &Inventory, mechanism: Mechanism) -> CkniObservation {
    let link = link_path(ck);
    let state = match fs::symlink_metadata(&link) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => CkniState::Missing,
        _ if !inventory.owns_path(CKNI_KIND, &link) => CkniState::Unowned,
        Err(_) => CkniState::Stale,
        Ok(_) if matches_ck(ck, &link, mechanism) => CkniState::Current,
        Ok(_) => CkniState::Stale,
    };
    CkniObservation {
        ck: ck.to_path_buf(),
        link,
        state,
    }
}

fn matches_ck(ck: &Path, link: &Path, mechanism: Mechanism) -> bool {
    match mechanism {
        Mechanism::Symlink => {
            fs::read_link(link).is_ok_and(|target| target == symlink_target(ck) || target == ck)
        }
        Mechanism::Copy => {
            let is_file = fs::symlink_metadata(link).is_ok_and(|meta| meta.file_type().is_file());
            is_file
                && matches!(
                    (digest_file(ck), digest_file(link)),
                    (Ok(ck), Ok(link)) if ck == link
                )
        }
    }
}

/// The symlink names `ck` by file name alone, so the pair keeps working if
/// the folder holding both is moved or reached through another path.
fn symlink_target(ck: &Path) -> PathBuf {
    PathBuf::from(ck.file_name().unwrap_or(ck.as_os_str()))
}

/// For a `ckni` row recorded as a symlink, the targets the link may hold
/// while it is still setup's: the recorded `links_to` path, and that path's
/// file name, which is the form [`install`] writes. Empty when the row
/// doesn't say where it pointed, so nothing at the path counts as setup's.
/// `None` for any row that isn't a symlink, such as a copy, which is judged
/// by its digest instead.
pub(crate) fn recorded_symlink_targets(entry: &Value) -> Option<Vec<PathBuf>> {
    if entry.get("mechanism").and_then(Value::as_str) != Some(Mechanism::Symlink.label()) {
        return None;
    }
    let Some(links_to) = entry.get("links_to").and_then(Value::as_str) else {
        return Some(Vec::new());
    };
    let links_to = PathBuf::from(links_to);
    let mut targets = vec![symlink_target(&links_to)];
    targets.push(links_to);
    Some(targets)
}

/// Puts `ckni` beside an owned `ck`, or brings an owned one back in line
/// with it, and records it in the manifest. Refuses an unowned `ckni` with
/// `unowned_refusal`, the message the calling command gives for any binary
/// it didn't install. Returns a line saying what it did, or `None` when the
/// `ckni` there was already current.
pub fn install(
    ck: &Path,
    inventory: &mut Inventory,
    mechanism: Mechanism,
    unowned_refusal: impl FnOnce(&Path) -> String,
) -> Result<Option<String>, String> {
    if !ck_is_owned(ck, inventory) {
        return Err(format!(
            "refusal: installer-manifest.json does not own ck at {}, so ckni is not placed beside it",
            ck.display()
        ));
    }
    let observed = observe(ck, inventory, mechanism);
    let link = observed.link;
    match observed.state {
        CkniState::Current => return Ok(None),
        CkniState::Unowned => return Err(unowned_refusal(&link)),
        CkniState::Missing | CkniState::Stale => {}
    }

    let mut fields = Map::new();
    fields.insert(
        "links_to".to_string(),
        Value::String(ck.to_string_lossy().into_owned()),
    );
    fields.insert(
        "mechanism".to_string(),
        Value::String(mechanism.label().to_string()),
    );
    let detail = match mechanism {
        Mechanism::Symlink => {
            place_symlink(ck, &link)?;
            format!(
                "linked {} to ck",
                super::components::display_home_path(&link)
            )
        }
        Mechanism::Copy => {
            place_copy(ck, &link, inventory)?;
            fields.insert("sha256".to_string(), Value::String(digest_file(&link)?));
            format!(
                "copied ck to {}",
                super::components::display_home_path(&link)
            )
        }
    };
    record(inventory, &link, fields)?;
    inventory.save()?;
    Ok(Some(detail))
}

/// Writes the row for `link`, replacing the fields of an existing row: a
/// re-recorded `ckni` may have changed mechanism or `ck` path, and a symlink
/// row must not keep a copy's `sha256`.
fn record(
    inventory: &mut Inventory,
    link: &Path,
    fields: Map<String, Value>,
) -> Result<(), String> {
    if !inventory.owns_path(CKNI_KIND, link) {
        inventory.record(CKNI_KIND, link, fields);
        return Ok(());
    }
    for key in ["links_to", "mechanism", "sha256"] {
        match fields.get(key).and_then(Value::as_str) {
            Some(value) => {
                inventory.update_owned_string(CKNI_KIND, link, key, value.to_string())?
            }
            None => inventory.remove_owned_string(CKNI_KIND, link, key)?,
        }
    }
    Ok(())
}

/// A fresh hidden name beside `link` for staging, so a leftover from an
/// interrupted run never blocks the next one.
fn staging_path(link: &Path) -> PathBuf {
    let name = link
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "ckni".to_string());
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    link.with_file_name(format!(".{name}.link-{}-{nonce}", process::id()))
}

/// Creates the symlink under a staging name and renames it over `link`, so
/// `ckni` either keeps its old target or has the new one, never neither.
#[cfg(unix)]
fn place_symlink(ck: &Path, link: &Path) -> Result<(), String> {
    let staged = staging_path(link);
    std::os::unix::fs::symlink(symlink_target(ck), &staged)
        .map_err(|error| format!("could not create ckni link {}: {error}", staged.display()))?;
    fs::rename(&staged, link).map_err(|error| {
        let _ = fs::remove_file(&staged);
        format!("could not put ckni link at {}: {error}", link.display())
    })
}

#[cfg(not(unix))]
fn place_symlink(_ck: &Path, link: &Path) -> Result<(), String> {
    Err(format!(
        "refusal: ckni at {} is recorded as a symlink, which this platform does not create",
        link.display()
    ))
}

/// Copies `ck` under a staging name, then renames the copy over `link`
/// through the same step that replaces managed binaries. On Windows a
/// running `ckni.exe` can't be overwritten but can be renamed, so that step
/// moves it aside first; the aside file is deleted here if Windows lets go
/// of it, and otherwise by the sweep a later setup or upgrade runs.
fn place_copy(ck: &Path, link: &Path, inventory: &mut Inventory) -> Result<(), String> {
    let staged = staging_path(link);
    file_modes::copy_file_with_mode(ck, &staged, 0o755).map_err(|error| {
        format!(
            "could not copy ck to {} for ckni: {error}",
            staged.display()
        )
    })?;
    let aside =
        managed_replace::place_staged_binary(&staged, link, inventory).inspect_err(|_| {
            let _ = fs::remove_file(&staged);
        })?;
    if let Some(aside) = aside {
        // A still-running ckni keeps its image; the row place_staged_binary
        // wrote keeps it on record for the later sweep.
        let _ = managed_replace::remove_replaced_image(inventory, &aside);
    }
    Ok(())
}

/// The engine's `ck` replacement step calls this after it puts new bytes at
/// `ck` (an upgrade or a rollback). A missing or stale `ckni` is brought in
/// line, which on Windows rewrites the copy. An unowned `ckni` is left alone:
/// the ownership check before the upgrade already refused it, so finding one
/// here means it appeared during the run, and replacing it would destroy
/// something this run didn't put there. Returns a note for the stage's
/// evidence.
pub(crate) fn refresh_after_ck_replacement(
    ck: &Path,
    inventory: &mut Inventory,
    mechanism: Mechanism,
) -> Result<String, String> {
    let observed = observe(ck, inventory, mechanism);
    if observed.state == CkniState::Unowned {
        return Ok(format!(
            "; ckni at {} left alone because ck setup did not install it",
            observed.link.display()
        ));
    }
    let done = install(ck, inventory, mechanism, |link| {
        super::text::upgrade_unowned_binaries_refusal(&[link.to_path_buf()])
    })?;
    Ok(match done {
        Some(detail) => format!("; {detail}"),
        None => "; ckni already current".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    struct Fixture {
        root: ScratchDir,
        ck: PathBuf,
        inventory: Inventory,
    }

    /// An owned `ck` with `bytes` in its own folder, the manifest kept
    /// outside that folder so tests can compare the folder's whole listing.
    fn fixture(name: &str, ck_name: &str, bytes: &[u8]) -> Fixture {
        let root = ScratchDir::new(name);
        let bin = root.join("cmd");
        fs::create_dir_all(&bin).unwrap();
        let ck = bin.join(ck_name);
        fs::write(&ck, bytes).unwrap();
        let mut inventory =
            Inventory::load(root.join("installer-manifest.json"), "linux-x64").unwrap();
        inventory.record("managed-binary", &ck, Map::new());
        Fixture {
            root,
            ck,
            inventory,
        }
    }

    fn entries(directory: &Path) -> Vec<String> {
        let mut names = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn refusal(path: &Path) -> String {
        super::super::text::unowned_binary_refusal(path)
    }

    #[test]
    fn the_link_is_named_after_cks_extension() {
        assert_eq!(
            link_path(Path::new("/opt/cmd/ck")),
            Path::new("/opt/cmd/ckni")
        );
        assert_eq!(
            link_path(Path::new("/opt/cmd/ck.exe")),
            Path::new("/opt/cmd/ckni.exe")
        );
        assert_eq!(
            link_path(Path::new("/opt/cmd/CK.EXE")),
            Path::new("/opt/cmd/ckni.exe")
        );
    }

    #[cfg(unix)]
    #[test]
    fn setup_links_ckni_to_ck_and_records_it_without_a_digest() {
        let mut f = fixture("ckni-symlink", "ck", b"ck bytes");
        let done = install(&f.ck, &mut f.inventory, Mechanism::Symlink, refusal).unwrap();
        let link = f.ck.with_file_name("ckni");
        assert!(done.unwrap().contains("linked"));
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("ck"));
        assert_eq!(fs::read(&link).unwrap(), b"ck bytes");
        assert_eq!(entries(f.ck.parent().unwrap()), ["ck", "ckni"]);

        let saved = Inventory::load(f.root.join("installer-manifest.json"), "linux-x64").unwrap();
        let row = saved.entry_for_path(CKNI_KIND, &link).unwrap();
        assert_eq!(row["mechanism"], "symlink");
        assert_eq!(row["links_to"], f.ck.to_string_lossy().as_ref());
        assert!(row.get("sha256").is_none(), "{row}");

        // Once in place, a second run changes nothing.
        assert_eq!(
            install(&f.ck, &mut f.inventory, Mechanism::Symlink, refusal).unwrap(),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_owned_link_pointing_elsewhere_is_put_back() {
        let mut f = fixture("ckni-symlink-stale", "ck", b"ck bytes");
        let link = f.ck.with_file_name("ckni");
        std::os::unix::fs::symlink("elsewhere", &link).unwrap();
        f.inventory.record(CKNI_KIND, &link, Map::new());
        assert_eq!(
            observe(&f.ck, &f.inventory, Mechanism::Symlink).state,
            CkniState::Stale
        );
        install(&f.ck, &mut f.inventory, Mechanism::Symlink, refusal).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("ck"));
        assert_eq!(entries(f.ck.parent().unwrap()), ["ck", "ckni"]);
    }

    #[test]
    fn the_copy_matches_ck_and_records_its_digest() {
        let mut f = fixture("ckni-copy", "ck.exe", b"ck bytes");
        let link = f.ck.with_file_name("ckni.exe");
        let done = install(&f.ck, &mut f.inventory, Mechanism::Copy, refusal).unwrap();
        assert!(done.unwrap().contains("copied"));
        assert_eq!(fs::read(&link).unwrap(), b"ck bytes");
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_file());
        let row = f.inventory.entry_for_path(CKNI_KIND, &link).unwrap();
        assert_eq!(row["mechanism"], "copy");
        assert_eq!(row["sha256"], digest_file(&f.ck).unwrap().as_str());
        let saved = Inventory::load(f.root.join("installer-manifest.json"), "linux-x64").unwrap();
        assert_eq!(saved.entry_for_path(CKNI_KIND, &link), Some(row));

        // A new ck makes the copy stale, and installing again rewrites it.
        fs::write(&f.ck, b"new ck bytes").unwrap();
        assert_eq!(
            observe(&f.ck, &f.inventory, Mechanism::Copy).state,
            CkniState::Stale
        );
        install(&f.ck, &mut f.inventory, Mechanism::Copy, refusal).unwrap();
        assert_eq!(fs::read(&link).unwrap(), b"new ck bytes");
        assert_eq!(
            f.inventory.entry_for_path(CKNI_KIND, &link).unwrap()["sha256"],
            digest_file(&f.ck).unwrap().as_str()
        );
        assert_eq!(entries(f.ck.parent().unwrap()), ["ck.exe", "ckni.exe"]);
    }

    #[test]
    fn an_unowned_ckni_is_refused_and_left_as_it_was() {
        let mut f = fixture("ckni-unowned", "ck", b"ck bytes");
        let link = f.ck.with_file_name("ckni");
        fs::write(&link, b"someone else's ckni").unwrap();
        for mechanism in [Mechanism::Symlink, Mechanism::Copy] {
            assert_eq!(
                observe(&f.ck, &f.inventory, mechanism).state,
                CkniState::Unowned
            );
            let error = install(&f.ck, &mut f.inventory, mechanism, refusal).unwrap_err();
            assert_eq!(error, refusal(&link));
            assert_eq!(fs::read(&link).unwrap(), b"someone else's ckni");
            assert!(!f.inventory.owns_path(CKNI_KIND, &link));
        }
        // The engine's replacement step leaves it alone too, and says so.
        let note = refresh_after_ck_replacement(&f.ck, &mut f.inventory, Mechanism::Copy).unwrap();
        assert!(note.contains("left alone"), "{note}");
        assert_eq!(fs::read(&link).unwrap(), b"someone else's ckni");
    }

    #[test]
    fn nothing_is_placed_beside_a_ck_the_manifest_does_not_own() {
        let root = ScratchDir::new("ckni-unowned-ck");
        let ck = root.join("ck");
        fs::write(&ck, b"a build").unwrap();
        let mut inventory =
            Inventory::load(root.join("installer-manifest.json"), "linux-x64").unwrap();
        let error = install(&ck, &mut inventory, Mechanism::Copy, refusal).unwrap_err();
        assert!(error.contains("does not own ck"), "{error}");
        assert!(!link_path(&ck).exists());
    }
}
