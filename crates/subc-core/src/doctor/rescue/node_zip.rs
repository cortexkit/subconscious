//! Bounded extraction of the pinned Windows Node distribution into private staging.

use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
};

struct Limits {
    paths: usize,
    bytes: u64,
}

pub(super) fn extract(archive: &Path, staging: &Path) -> io::Result<()> {
    // The Node distribution is much smaller than these limits, even including npm.
    extract_with_limits(
        File::open(archive)?,
        staging,
        Limits {
            paths: 50_000,
            bytes: 512 * 1024 * 1024,
        },
    )
}

fn extract_with_limits(
    archive: impl Read + io::Seek,
    staging: &Path,
    mut remaining: Limits,
) -> io::Result<()> {
    // PowerShell's Expand-Archive can silently ignore paths with the Windows \\?\ prefix.
    // Open the exact files in staging instead of rewriting the validated paths.
    let mut archive = zip::ZipArchive::new(archive)?;
    if archive.len() > remaining.paths {
        return Err(invalid("too many paths in the Node ZIP"));
    }
    let mut directories = BTreeSet::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let name = entry.name();
        let path = entry.enclosed_name().filter(|path| {
            path.file_name().is_some()
                && !name.starts_with(['/', '\\'])
                && !name.contains(':')
                && !name.split(['/', '\\']).any(|part| part == "..")
        });
        let path = path.ok_or_else(|| invalid("invalid entry path in the Node ZIP"))?;
        if entry.is_symlink() {
            return Err(invalid("symlink entry in the Node ZIP"));
        }
        let directory = entry.is_dir();
        let size = entry.size();
        if size > remaining.bytes || (directory && size != 0) {
            return Err(invalid("invalid uncompressed size in the Node ZIP"));
        }
        let parent = if directory {
            &path
        } else {
            path.parent().expect("entry path has a filename")
        };
        create_parents(staging, parent, &mut directories, &mut remaining.paths)?;
        if !directory {
            charge_path(&mut remaining.paths)?;
            let mut file = File::create_new(staging.join(path))?;
            let written = io::copy(&mut (&mut entry).take(size), &mut file)?;
            // Do not trust the declared size. Reading past the copy limit also
            // lets the ZIP reader check the checksum at the end of the entry.
            if written != size || entry.read(&mut [0])? != 0 {
                return Err(invalid("entry size does not match the Node ZIP"));
            }
            remaining.bytes -= written;
        }
    }
    Ok(())
}

fn create_parents(
    staging: &Path,
    parent: &Path,
    directories: &mut BTreeSet<PathBuf>,
    paths: &mut usize,
) -> io::Result<()> {
    // Count implicit parent directories as well as paths listed in the archive.
    let mut relative = PathBuf::new();
    for part in parent.components() {
        if part == std::path::Component::CurDir {
            continue;
        }
        relative.push(part);
        if !directories.contains(&relative) {
            charge_path(paths)?;
            fs::create_dir(staging.join(&relative))?;
            directories.insert(relative.clone());
        }
    }
    Ok(())
}

fn charge_path(paths: &mut usize) -> io::Result<()> {
    *paths = paths
        .checked_sub(1)
        .ok_or_else(|| invalid("too many paths in the Node ZIP"))?;
    Ok(())
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use cortexkit_test_support::ScratchDir;
    use zip::{write::SimpleFileOptions, ZipWriter};

    use super::*;

    fn fixture(build: impl FnOnce(&mut ZipWriter<Cursor<Vec<u8>>>)) -> Cursor<Vec<u8>> {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        build(&mut zip);
        let mut archive = zip.finish().unwrap();
        archive.set_position(0);
        archive
    }

    fn file(zip: &mut ZipWriter<Cursor<Vec<u8>>>, name: &str, content: &[u8]) {
        zip.start_file(name, SimpleFileOptions::default()).unwrap();
        zip.write_all(content).unwrap();
    }

    #[test]
    fn node_zip_refuses_unsafe_paths_and_symlinks_before_later_entries() {
        let root = ScratchDir::new("doctor-node-zip-unsafe");
        for name in [
            "/absolute",
            "C:\\absolute",
            "node/../escaped",
            "../../escaped",
            "file:stream",
        ] {
            let zip = fixture(|zip| {
                file(zip, name, b"blocked");
                file(zip, "later", b"must not be extracted");
            });
            let error = extract_with_limits(
                zip,
                root.path(),
                Limits {
                    paths: 16,
                    bytes: 256,
                },
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("invalid entry path"),
                "{name}: {error}"
            );
            assert!(fs::read_dir(root.path()).unwrap().next().is_none());
        }
        let zip = fixture(|zip| {
            zip.add_symlink("link", "target", SimpleFileOptions::default())
                .unwrap();
            file(zip, "later", b"must not be extracted");
        });
        let error = extract_with_limits(
            zip,
            root.path(),
            Limits {
                paths: 16,
                bytes: 256,
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("symlink entry"), "{error}");
        assert!(fs::read_dir(root.path()).unwrap().next().is_none());
    }

    #[test]
    fn node_zip_bounds_archive_entries_implicit_parents_and_total_bytes() {
        let root = ScratchDir::new("doctor-node-zip-limits");
        let zip = fixture(|zip| {
            file(zip, "first", b"ab");
            file(zip, "second", b"cd");
        });
        let error = extract_with_limits(zip.clone(), root.path(), Limits { paths: 1, bytes: 4 })
            .unwrap_err();
        assert!(error.to_string().contains("too many paths"), "{error}");
        assert!(!root.join("first").exists());

        let nested = fixture(|zip| file(zip, "a/b/c", b"x"));
        let error =
            extract_with_limits(nested, root.path(), Limits { paths: 2, bytes: 4 }).unwrap_err();
        assert!(error.to_string().contains("too many paths"), "{error}");
        assert!(!root.join("a/b/c").exists());

        let error =
            extract_with_limits(zip, root.path(), Limits { paths: 2, bytes: 3 }).unwrap_err();
        assert!(error.to_string().contains("uncompressed size"), "{error}");
        assert_eq!(fs::read(root.join("first")).unwrap(), b"ab");
        assert!(!root.join("second").exists());
    }

    #[test]
    fn node_zip_does_not_replace_an_existing_file() {
        let root = ScratchDir::new("doctor-node-zip-existing");
        let path = root.join("node.exe");
        fs::write(&path, b"previous contents").unwrap();
        let zip = fixture(|zip| file(zip, "node.exe", b"replacement"));
        let error = extract_with_limits(
            zip,
            root.path(),
            Limits {
                paths: 1,
                bytes: 16,
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(path).unwrap(), b"previous contents");
    }
}
