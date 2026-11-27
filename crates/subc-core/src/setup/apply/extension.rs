use super::super::{
    file_modes,
    install_journal::{self, files},
    release_index,
};
use super::Inventory;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub(super) const ASSET: &str = "ck-cerebellum-extension";
pub(in crate::setup) const KIND: &str = "cerebellum-extension";
const ZIP: &str = "zip_sha256";
const TREE: &str = "tree_digest";
const PENDING_ZIP: &str = "pending_zip_sha256";
const PENDING_TREE: &str = "pending_tree_digest";

#[cfg(test)]
#[path = "extension_tests.rs"]
mod tests;

pub(in crate::setup) struct Paths {
    pub live: PathBuf,
    pub new: PathBuf,
    pub old: PathBuf,
}

impl Paths {
    pub fn new(data: &Path) -> Self {
        Self {
            live: data.join(KIND),
            new: data.join(format!("{KIND}.new")),
            old: data.join(format!("{KIND}.old")),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Row {
    zip: Option<String>,
    tree: Option<String>,
    pending_zip: Option<String>,
    pending_tree: Option<String>,
}

impl Row {
    fn read(paths: &Paths, inventory: &Inventory) -> Result<Self, String> {
        let Some(row) = inventory.entry_for_path(KIND, &paths.live) else {
            return Ok(Self::default());
        };
        let get = |name| match row.get(name) {
            None => Ok(None),
            Some(value) => value
                .as_str()
                .filter(|value| {
                    value.len() == 64
                        && value
                            .bytes()
                            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                })
                .map(|value| Some(value.to_owned()))
                .ok_or_else(|| refusal(&paths.live, format!("invalid inventory field {name}"))),
        };
        let row = Self {
            zip: get(ZIP)?,
            tree: get(TREE)?,
            pending_zip: get(PENDING_ZIP)?,
            pending_tree: get(PENDING_TREE)?,
        };
        if row.zip.is_some() != row.tree.is_some()
            || (row.pending_tree.is_some() && row.pending_zip.is_none())
        {
            return Err(refusal(&paths.live, "incomplete extension inventory row"));
        }
        Ok(row)
    }

    fn save(&self, paths: &Paths, inventory: &mut Inventory) -> Result<(), String> {
        let fields: Map<String, Value> = [
            (ZIP, &self.zip),
            (TREE, &self.tree),
            (PENDING_ZIP, &self.pending_zip),
            (PENDING_TREE, &self.pending_tree),
        ]
        .into_iter()
        .filter_map(|(field, value)| {
            value
                .as_ref()
                .map(|value| (field.into(), Value::String(value.clone())))
        })
        .collect();
        if fields.is_empty() {
            inventory.drop_retained(KIND, &paths.live)
        } else {
            inventory.persist_retained(KIND, &paths.live, fields)
        }
    }
}

fn refusal(path: &Path, detail: impl std::fmt::Display) -> String {
    format!("refusal: Chrome extension {}: {detail}", path.display())
}

fn metadata(path: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(info) => Ok(Some(info)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(refusal(path, error)),
    }
}

pub(in crate::setup) fn tree_digest(root: &Path) -> Result<Option<String>, String> {
    let Some(info) = metadata(root)? else {
        return Ok(None);
    };
    if !info.is_dir() || install_journal::is_reparse_point(&info) {
        return Err(refusal(root, "not a plain directory"));
    }
    let mut entries = Vec::new();
    walk(root, &[], &mut entries)?;
    entries.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
    let mut digest = Sha256::new();
    for (name, file_hash) in entries {
        if let Some(hash) = file_hash {
            digest.update(b"f ");
            digest.update(hash);
            digest.update(b"  ");
        } else {
            digest.update(b"d  ");
        }
        digest.update(name);
        digest.update(b"\n");
    }
    Ok(Some(format!("{:x}", digest.finalize())))
}

fn walk(
    dir: &Path,
    prefix: &[u8],
    entries: &mut Vec<(Vec<u8>, Option<String>)>,
) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|error| refusal(dir, error))? {
        let entry = entry.map_err(|error| refusal(dir, error))?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.as_encoded_bytes();
        if name.contains(&b'\n') || name.contains(&b'\r') {
            return Err(refusal(&path, "a path contains a line break"));
        }
        let mut relative = prefix.to_vec();
        if !relative.is_empty() {
            relative.push(b'/');
        }
        relative.extend_from_slice(name);
        let info = metadata(&path)?.ok_or_else(|| refusal(&path, "entry disappeared"))?;
        if install_journal::is_reparse_point(&info) || info.file_type().is_symlink() {
            return Err(refusal(&path, "links are not allowed"));
        } else if info.is_dir() {
            walk(&path, &relative, entries)?;
            entries.push((relative, None));
        } else if info.is_file() {
            let hash = files::digest(&path)?.ok_or_else(|| refusal(&path, "file disappeared"))?;
            entries.push((relative, Some(hash)));
        } else {
            return Err(refusal(&path, "not a regular file or directory"));
        }
    }
    Ok(())
}

struct State {
    live: Option<String>,
    old: bool,
    new: bool,
}

fn observe(paths: &Paths, row: &Row) -> Result<State, String> {
    let new = metadata(&paths.new)?.is_some();
    let old = metadata(&paths.old)?.is_some();
    if row.pending_zip.is_none() {
        for (exists, path) in [(new, &paths.new), (old, &paths.old)] {
            if exists {
                return Err(refusal(path, "not owned (no pending extension swap)"));
            }
        }
    }
    let live = tree_digest(&paths.live)?;
    if let Some(digest) = &live {
        if Some(digest) != row.tree.as_ref() && Some(digest) != row.pending_tree.as_ref() {
            return Err(refusal(
                &paths.live,
                "tree does not match the installation inventory",
            ));
        }
    }
    if old
        && (row.tree.is_none()
            || tree_digest(&paths.old)? != row.tree
            || (live.is_some() && live != row.pending_tree))
    {
        return Err(refusal(
            &paths.old,
            "backup does not match a recoverable extension swap",
        ));
    }
    Ok(State { live, old, new })
}

pub(super) fn plan(paths: &Paths, inventory: &Inventory, zip: &str) -> Result<bool, String> {
    let row = Row::read(paths, inventory)?;
    let state = observe(paths, &row)?;
    Ok(row.pending_zip.is_some() || state.live.is_none() || row.zip.as_deref() != Some(zip))
}

pub(super) fn check_ownership(paths: &Paths, inventory: &Inventory) -> Result<(), String> {
    observe(paths, &Row::read(paths, inventory)?)?;
    Ok(())
}

fn remove(path: &Path) -> Result<(), String> {
    let Some(info) = metadata(path)? else {
        return Ok(());
    };
    let result = if info.is_dir() && !install_journal::is_reparse_point(&info) {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    result.map_err(|error| refusal(path, error))?;
    install_journal::sync_parent(path)
}

fn rename(from: &Path, to: &Path) -> Result<(), String> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use rustix::fs::{renameat_with, RenameFlags, CWD};
        renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE)
            .map_err(|error| refusal(to, error))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        if metadata(to)?.is_some() {
            return Err(refusal(to, "rename destination already exists"));
        }
        fs::rename(from, to).map_err(|error| refusal(to, error))
    }
}

/// Reconcile the extension's own commit marker before setup or uninstall.
pub(in crate::setup) fn recover(paths: &Paths, inventory: &mut Inventory) -> Result<(), String> {
    let mut row = Row::read(paths, inventory)?;
    let state = observe(paths, &row)?;
    if row.pending_zip.is_none() {
        return Ok(());
    }
    if state.live.is_some() && state.live == row.pending_tree {
        if state.old {
            remove(&paths.old)?;
        }
        row.zip = row.pending_zip.take();
        row.tree = row.pending_tree.take();
    } else if state.live.is_none() && state.old {
        rename(&paths.old, &paths.live)?;
        install_journal::sync_parent(&paths.live)?;
    }
    if state.new {
        remove(&paths.new)?;
    }
    row.pending_zip = None;
    row.pending_tree = None;
    row.save(paths, inventory)
}

pub(super) fn ensure(
    paths: &Paths,
    inventory: &mut Inventory,
    url: &str,
    zip: &str,
) -> Result<bool, String> {
    ensure_with(paths, inventory, zip, |to| release_index::download(url, to))
}

fn ensure_with(
    paths: &Paths,
    inventory: &mut Inventory,
    zip: &str,
    download: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<bool, String> {
    let before = Row::read(paths, inventory)?;
    recover(paths, inventory)?;
    let previous = Row::read(paths, inventory)?;
    if metadata(&paths.live)?.is_some() && previous.zip.as_deref() == Some(zip) {
        return Ok(before != previous);
    }
    let workspace = Download::new(paths.live.parent().expect("extension parent"))?;
    let archive = workspace.0.join("extension.zip");
    download(&archive)?;
    let mut archive_file =
        subc_os::open_regular_nofollow(&archive).map_err(|error| refusal(&archive, error))?;
    let mut hash = Sha256::new();
    io::copy(&mut archive_file, &mut hash).map_err(|error| refusal(&archive, error))?;
    let digest = format!("{:x}", hash.finalize());
    if digest != zip {
        return Err(refusal(
            &paths.live,
            format!("ZIP SHA-256 mismatch: expected {zip}, found {digest}"),
        ));
    }
    // The open file is sufficient for extraction. A process crash during the
    // swap must not leave a second staging directory beside the extension.
    fs::remove_dir_all(&workspace.0).map_err(|error| refusal(&workspace.0, error))?;
    let mut pending = previous.clone();
    pending.pending_zip = Some(zip.into());
    let mut owns_new = false;
    let mut swapped = false;
    let result = (|| {
        pending.save(paths, inventory)?;
        #[cfg(feature = "test-support")]
        install_journal::crash_hook("extension-zip-pending");
        file_modes::create_private_directory(&paths.new)?;
        owns_new = true;
        extract(archive_file, &paths.new)?;
        pending.pending_tree = tree_digest(&paths.new)?;
        if pending.pending_tree.is_none() {
            return Err(refusal(&paths.new, "staging tree disappeared"));
        }
        pending.save(paths, inventory)?;
        let state = observe(paths, &pending)?;
        if state.old {
            return Err(refusal(
                &paths.old,
                "backup appeared before the extension swap",
            ));
        }
        // Once a rename succeeds, keep the pending row on any error. Recovery
        // distinguishes the old tree, the rename gap, and a published new tree.
        if state.live.is_some() {
            rename(&paths.live, &paths.old)?;
            swapped = true;
            install_journal::sync_parent(&paths.old)?;
            #[cfg(feature = "test-support")]
            install_journal::crash_hook("extension-old-renamed");
        }
        rename(&paths.new, &paths.live)?;
        swapped = true;
        install_journal::sync_parent(&paths.live)?;
        #[cfg(feature = "test-support")]
        install_journal::crash_hook("extension-new-published");
        recover(paths, inventory)?;
        Ok(true)
    })();
    match result {
        Err(error) if !swapped => {
            if owns_new {
                remove(&paths.new)
                    .map_err(|cleanup| format!("{error}; staging cleanup failed: {cleanup}"))?;
            }
            previous
                .save(paths, inventory)
                .map_err(|cleanup| format!("{error}; inventory cleanup failed: {cleanup}"))?;
            Err(error)
        }
        result => result,
    }
}

struct Download(PathBuf);

impl Download {
    fn new(parent: &Path) -> Result<Self, String> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| refusal(parent, error))?
            .as_nanos();
        let path = parent.join(format!(".ck-extension-{}-{nonce}", std::process::id()));
        file_modes::create_private_directory(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Download {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn extract(file: fs::File, destination: &Path) -> Result<(), String> {
    let mut directory = file
        .try_clone()
        .map_err(|error| refusal(destination, error))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|error| refusal(destination, error))?;
    reject_duplicate_headers(
        &mut directory,
        archive.central_directory_start(),
        archive.len(),
    )
    .map_err(|error| refusal(destination, error))?;
    let mut names = BTreeSet::new();
    let mut entries = Vec::new();
    let mut has_manifest = false;
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|error| refusal(destination, error))?;
        let raw = entry.name_raw();
        let name = std::str::from_utf8(raw).map_err(|error| refusal(destination, error))?;
        // Do not let platform-specific separators, NULs or line breaks alter
        // the path interpreted by the filesystem or the tree digest format.
        if name.starts_with('/')
            || raw
                .iter()
                .any(|byte| matches!(byte, b'\\' | b':' | b'\0' | b'\r' | b'\n'))
        {
            return Err(refusal(destination, format!("unsafe ZIP entry {name:?}")));
        }
        let clean = name.strip_suffix('/').unwrap_or(name);
        if clean.split('/').any(|part| matches!(part, "" | "." | "..")) {
            return Err(refusal(destination, format!("unsafe ZIP entry {name:?}")));
        }
        let directory = entry.is_dir();
        let kind = entry.unix_mode().unwrap_or(0) & 0o170000;
        if entry.is_symlink()
            || !matches!(
                (directory, kind),
                (_, 0) | (true, 0o040000) | (false, 0o100000)
            )
        {
            return Err(refusal(
                destination,
                format!("ZIP entry {name:?} is not a regular file or directory"),
            ));
        }
        if !names.insert(clean.to_owned()) {
            return Err(refusal(
                destination,
                format!("duplicate ZIP entry {name:?}"),
            ));
        }
        has_manifest |= !directory && clean == "manifest.json";
        entries.push((clean.to_owned(), directory));
    }
    if !has_manifest {
        return Err(refusal(destination, "ZIP has no root manifest.json"));
    }
    for (index, (name, directory)) in entries.iter().enumerate() {
        let path = destination.join(name);
        if *directory {
            fs::create_dir_all(&path).map_err(|error| refusal(&path, error))?;
        } else {
            let parent = path.parent().expect("validated archive path");
            fs::create_dir_all(parent).map_err(|error| refusal(parent, error))?;
            let mut from = archive
                .by_index(index)
                .map_err(|error| refusal(&path, error))?;
            let mut to = fs::File::create_new(&path).map_err(|error| refusal(&path, error))?;
            io::copy(&mut from, &mut to)
                .and_then(|_| to.sync_all())
                .map_err(|error| refusal(&path, error))?;
            install_journal::sync_parent(&path)?;
        }
    }
    let manifest = destination.join("manifest.json");
    let file =
        subc_os::open_regular_nofollow(&manifest).map_err(|error| refusal(&manifest, error))?;
    serde_json::from_reader::<_, Value>(file).map_err(|error| refusal(&manifest, error))?;
    install_journal::sync_parent(&manifest)
}

pub(in crate::setup) fn reject_duplicate_headers(
    reader: &mut fs::File,
    start: u64,
    unique: usize,
) -> io::Result<()> {
    // ZipArchive indexes entries by decoded name and silently coalesces
    // duplicates. Count the original central file headers at its parsed
    // offset so a hidden entry cannot escape the checks below.
    const FILE_HEADER: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
    reader.seek(SeekFrom::Start(start))?;
    for _ in 0..unique {
        let mut header = [0; 46];
        reader.read_exact(&mut header)?;
        if header[..4] != FILE_HEADER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid ZIP central file header",
            ));
        }
        let trailing = [28, 30, 32]
            .into_iter()
            .map(|i| i64::from(u16::from_le_bytes([header[i], header[i + 1]])))
            .sum();
        reader.seek(SeekFrom::Current(trailing))?;
    }
    let mut next = [0; 4];
    reader.read_exact(&mut next)?;
    if next == FILE_HEADER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "duplicate ZIP entry",
        ));
    }
    Ok(())
}
