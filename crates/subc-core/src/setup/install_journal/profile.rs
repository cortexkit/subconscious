//! The shell profile stays user-owned. Setup only replaces the bytes delimited
//! by its own markers, and refuses an ambiguous or malformed block.

use super::{bootstrap, files, transaction::Transaction};
use serde_json::{Map, Value};
use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

pub(super) const BEGIN: &str = "# cortexkit-managed PATH begin";
pub(super) const END: &str = "# cortexkit-managed PATH end";

pub(in crate::setup) fn from_bootstrap(manifest: &Path, running: &Path) -> Result<(), String> {
    let Some(profile) = env::var_os("CK_BOOTSTRAP_PROFILE") else {
        return Ok(());
    };
    let fish = match env::var("CK_BOOTSTRAP_PROFILE_KIND").as_deref() {
        Ok("fish") => true,
        Ok("posix") => false,
        _ => return Err("refusal: bootstrap supplied an unknown shell profile kind".into()),
    };
    apply(manifest, running, &PathBuf::from(profile), fish)
}

fn apply(manifest: &Path, running: &Path, profile: &Path, fish: bool) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let root = manifest
        .parent()
        .ok_or("installer manifest has no parent")?;
    let ck = bootstrap::destination(root);
    if !running
        .canonicalize()
        .ok()
        .zip(ck.canonicalize().ok())
        .is_some_and(|(running, ck)| running == ck)
    {
        return Err(format!(
            "refusal: bootstrap profile changes must run from {}",
            ck.display()
        ));
    }
    let path = resolve(profile)?;
    let mut inventory = bootstrap::read_inventory(manifest)?;
    let (before, mode) = match subc_os::open_regular_nofollow(&path) {
        Ok(mut file) => {
            use std::io::Read as _;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|error| message(&path, error))?;
            (
                bytes,
                file.metadata()
                    .map_err(|error| message(&path, error))?
                    .permissions()
                    .mode()
                    & 0o777,
            )
        }
        Err(subc_os::OpenRegularError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            (Vec::new(), 0o600)
        }
        Err(error) => return Err(message(&path, error)),
    };
    let after = path_block(
        &before,
        ck.parent().expect("bootstrap command directory"),
        fish,
    )
    .map_err(|error| message(&path, error))?;
    let mut transaction = Transaction::begin(manifest)?;
    let steps = (|| {
        if after != before {
            transaction.write(&path, &after, mode)?;
        }
        inventory.record(
            "shell-profile-path",
            &path,
            Map::from_iter([
                ("begin_marker".into(), Value::String(BEGIN.into())),
                ("end_marker".into(), Value::String(END.into())),
            ]),
        );

        let legacy = root.join("bin/ck");
        // A legacy path is eligible only inside our managed data directory,
        // never a shadowing ck elsewhere on PATH or outside a symlinked bin.
        if fs::symlink_metadata(root.join("bin")).is_ok_and(|info| info.is_dir())
            && fs::symlink_metadata(&legacy)
                .is_ok_and(|info| info.is_file() && info.permissions().mode() & 0o111 != 0)
        {
            let tag = transaction.id().to_owned();
            let backup = legacy.with_file_name(format!("ck.retired-{tag}"));
            if fs::symlink_metadata(&backup).is_ok() {
                return Err(message(&backup, "retirement path already exists"));
            }
            let digest =
                files::digest(&legacy)?.ok_or_else(|| message(&legacy, "legacy ck disappeared"))?;
            transaction.replace(&backup, &legacy)?;
            transaction.remove(&legacy)?;
            for kind in crate::setup::components::OWNED_BINARY_KINDS {
                inventory.remove_owned_path(kind, &legacy);
            }
            inventory.record(
                "legacy-cli-retirement",
                &backup,
                Map::from_iter([
                    (
                        "original_path".into(),
                        Value::String(legacy.to_string_lossy().into_owned()),
                    ),
                    (
                        "backup_path".into(),
                        Value::String(backup.to_string_lossy().into_owned()),
                    ),
                    ("sha256".into(), Value::String(digest)),
                ]),
            );
        }
        Ok::<_, String>(())
    })();
    let mut recovery = bootstrap::FileRecovery;
    if let Err(error) = steps {
        if let Err(undo) = transaction.undo(&mut recovery) {
            return Err(format!("{error}; recovery stopped: {undo}"));
        }
        return Err(error);
    }
    transaction.commit(&inventory, &mut recovery)?;
    Ok(())
}

fn resolve(path: &Path) -> Result<PathBuf, String> {
    let mut resolved = std::path::absolute(path).map_err(|error| message(path, error))?;
    for _ in 0..40 {
        match fs::read_link(&resolved) {
            Ok(link) if link.is_absolute() => resolved = link,
            Ok(link) => {
                resolved = resolved
                    .parent()
                    .ok_or_else(|| message(path, "profile has no parent"))?
                    .join(link)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::InvalidInput | io::ErrorKind::NotFound
                ) =>
            {
                return Ok(resolved)
            }
            Err(error) => return Err(message(path, error)),
        }
    }
    Err(message(path, "too many shell profile links"))
}

fn message(path: &Path, detail: impl std::fmt::Display) -> String {
    format!("refusal: bootstrap profile {}: {detail}", path.display())
}

pub(super) fn path_block(
    profile: &[u8],
    command_dir: &Path,
    fish: bool,
) -> Result<Vec<u8>, String> {
    let dir = command_dir
        .to_str()
        .ok_or("command directory is not UTF-8")?;
    let quoted = quote_path(dir, fish);
    let command = if fish {
        format!("set -gx PATH \"{quoted}\" $PATH")
    } else {
        format!("export PATH=\"{quoted}:$PATH\"")
    };
    let adding = format!("{BEGIN}\n{command}\n{END}\n");
    let mut begin = None;
    let mut removal = None;
    let mut position = 0;
    for terminated in profile.split_inclusive(|&byte| byte == b'\n') {
        let line = terminated.strip_suffix(b"\n").unwrap_or(terminated);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let next = position + terminated.len();
        if line == BEGIN.as_bytes() {
            if begin.is_some() || removal.is_some() {
                return Err("malformed CortexKit PATH markers".into());
            }
            begin = Some(position);
        }
        if line == END.as_bytes() {
            let start = begin.take().ok_or("malformed CortexKit PATH markers")?;
            removal = Some(start..next);
        }
        position = next;
    }
    if begin.is_some() {
        return Err("malformed CortexKit PATH markers".into());
    }

    let mut updated = Vec::new();
    if let Some(old) = removal {
        updated.extend(&profile[..old.start]);
        updated.extend(adding.as_bytes());
        updated.extend(&profile[old.end..]);
    } else {
        updated.extend(profile);
        if !updated.is_empty() && !updated.ends_with(b"\n") {
            updated.push(b'\n');
        }
        updated.extend(adding.as_bytes());
    }
    Ok(updated)
}

fn quote_path(directory: &str, fish: bool) -> String {
    let mut quoted = String::new();
    for character in directory.chars() {
        if matches!(character, '\\' | '"' | '$') || (!fish && character == '`') {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_only_the_owned_block_and_preserves_surrounding_bytes() {
        let old = [
            b"unchanged\r\n".as_slice(),
            format!("{BEGIN}\nold\n{END}\n").as_bytes(),
            b"non-UTF-8: \xff",
        ]
        .concat();
        let expected = [
            b"unchanged\r\n".as_slice(),
            format!("{BEGIN}\nexport PATH=\"/opt/\u{241f}\\$home:$PATH\"\n{END}\n").as_bytes(),
            b"non-UTF-8: \xff",
        ]
        .concat();
        assert_eq!(
            path_block(&old, Path::new("/opt/\u{241f}$home"), false).unwrap(),
            expected
        );
        assert_eq!(
            path_block(&expected, Path::new("/opt/\u{241f}$home"), false).unwrap(),
            expected
        );
    }

    #[test]
    fn refuses_missing_or_duplicate_markers() {
        for old in [
            format!("{BEGIN}\nunfinished"),
            format!("{END}\n"),
            format!("{BEGIN}\n{END}\n{BEGIN}\n{END}\n"),
        ] {
            assert!(path_block(old.as_bytes(), Path::new("/opt/bin"), true).is_err());
        }
    }

    #[test]
    fn profile_and_owned_legacy_retirement_commit_together() {
        use std::os::unix::fs::PermissionsExt as _;
        let fixture = cortexkit_test_support::ScratchDir::new("bootstrap-profile");
        let root = fs::canonicalize(fixture.path()).unwrap();
        let manifest = root.join("installer-manifest.json");
        let _lock =
            crate::setup::InstallHold::take(&manifest, "ck setup", std::time::Duration::ZERO)
                .unwrap();
        let ck = bootstrap::destination(&root);
        fs::create_dir(ck.parent().unwrap()).unwrap();
        fs::write(&ck, b"current executable").unwrap();
        let old = root.join("bin/ck");
        fs::create_dir(old.parent().unwrap()).unwrap();
        fs::write(&old, b"legacy executable").unwrap();
        fs::set_permissions(&old, fs::Permissions::from_mode(0o755)).unwrap();
        let profile = root.join("actual-profile");
        fs::write(&profile, b"outside the installer's block\n").unwrap();
        let link = root.join(".bashrc");
        std::os::unix::fs::symlink("actual-profile", &link).unwrap();
        apply(&manifest, &ck, &link, false).unwrap();
        assert!(link.is_symlink());
        assert!(fs::read_to_string(&profile)
            .unwrap()
            .starts_with("outside the installer's block\n# cortexkit-managed PATH begin\n"));
        assert!(!old.exists());
        let owned = bootstrap::read_inventory(&manifest).unwrap();
        assert!(owned.owns_path("shell-profile-path", &profile));
        let [backup] = &owned.paths_for_kind("legacy-cli-retirement")[..] else {
            panic!("expected one retired legacy executable")
        };
        assert_eq!(fs::read(backup).unwrap(), b"legacy executable");
    }
}
