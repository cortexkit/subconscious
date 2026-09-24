//! `spawn_cursor.json`: the cursor of the last spawn event the consumer processed.
//!
//! The value is the daemon's cursor verbatim (`{"daemon_incarnation", "seq"}`), written
//! after the event it names has been handled, so a restart resumes at the first event
//! it had not finished. Every write is an atomic durable replacement: a sibling `*.tmp`,
//! fsynced, renamed over the file, the directory fsynced. A file that does not parse as
//! a cursor is damaged and reads as absent, so the consumer snapshots and reconciles
//! instead of resuming; the next processed event overwrites it.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use subc_client_rs::consumer::SpawnCursor;

pub const CURSOR_FILE: &str = "spawn_cursor.json";

/// What reading the cursor file found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorRead {
    Absent,
    Present(SpawnCursor),
    /// Unreadable, or not a cursor. Treated as absent.
    Damaged {
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct CursorStore {
    path: PathBuf,
}

impl CursorStore {
    pub fn new(store_root: &Path) -> Self {
        Self {
            path: store_root.join(CURSOR_FILE),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Removes a `*.tmp` left by a write that died before its rename. It was never the
    /// file of record.
    pub fn remove_stale_tmp(&self) {
        let _ = fs::remove_file(tmp_path(&self.path));
    }

    pub fn read(&self) -> CursorRead {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return CursorRead::Absent,
            Err(error) => {
                return CursorRead::Damaged {
                    reason: format!("unreadable: {error}"),
                }
            }
        };
        match serde_json::from_slice::<SpawnCursor>(&bytes) {
            Ok(cursor) => CursorRead::Present(cursor),
            Err(error) => CursorRead::Damaged {
                reason: format!("not a spawn cursor: {error}"),
            },
        }
    }

    pub fn write(&self, cursor: &SpawnCursor) -> io::Result<()> {
        let bytes = serde_json::to_vec(cursor).map_err(io::Error::other)?;
        let tmp = tmp_path(&self.path);
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        if let Some(dir) = self.path.parent() {
            sync_dir(dir)?;
        }
        Ok(())
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// Windows cannot open a directory as a file to flush it; the rename is the durable
/// step there.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CursorRead, CursorStore};
    use subc_client_rs::consumer::SpawnCursor;

    #[test]
    fn a_written_cursor_reads_back_and_damage_reads_as_damaged() {
        let dir = std::env::temp_dir().join(format!(
            "ckbus-spawn-cursor-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = CursorStore::new(&dir);
        let _ = std::fs::remove_file(store.path());
        assert_eq!(store.read(), CursorRead::Absent);
        let cursor = SpawnCursor {
            daemon_incarnation: "incarnation-a".to_string(),
            seq: 12,
        };
        store.write(&cursor).unwrap();
        assert_eq!(store.read(), CursorRead::Present(cursor));
        std::fs::write(store.path(), b"{\"daemon_incarn").unwrap();
        assert!(matches!(store.read(), CursorRead::Damaged { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
