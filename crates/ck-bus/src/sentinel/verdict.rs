//! `sentinel_verdict.json`: the last verdict the sentinel reached, and the incarnation id
//! of the process that reached it.
//!
//! The file is a record for the operator and for the next process's start-up log. It
//! never answers health: a verdict describes the process that measured it, and a new
//! process starts down until its own first probe answers. Every write is an atomic
//! durable replacement (a sibling `*.tmp`, fsynced, renamed over the file, the directory
//! fsynced). A file that does not parse is damaged and reads as absent, which is already
//! a new process's start state.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use serde_json::Value;

pub const VERDICT_FILE: &str = "sentinel_verdict.json";

/// What reading the verdict file found.
#[derive(Debug, Clone, PartialEq)]
pub enum VerdictRead {
    Absent,
    Present(Value),
    /// Unreadable, or not a JSON object naming an incarnation. Treated as absent.
    Damaged {
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct VerdictStore {
    path: PathBuf,
}

impl VerdictStore {
    pub fn new(store_root: &Path) -> Self {
        Self {
            path: store_root.join(VERDICT_FILE),
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

    pub fn read(&self) -> VerdictRead {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return VerdictRead::Absent,
            Err(error) => {
                return VerdictRead::Damaged {
                    reason: format!("unreadable: {error}"),
                }
            }
        };
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) if value["incarnation"].is_string() => VerdictRead::Present(value),
            Ok(_) => VerdictRead::Damaged {
                reason: "not a verdict naming its incarnation".to_string(),
            },
            Err(error) => VerdictRead::Damaged {
                reason: format!("not JSON: {error}"),
            },
        }
    }

    pub fn write(&self, verdict: &Value) -> io::Result<()> {
        let bytes = serde_json::to_vec(verdict).map_err(io::Error::other)?;
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
    use super::{VerdictRead, VerdictStore, VERDICT_FILE};
    use serde_json::json;

    #[test]
    fn a_written_verdict_reads_back_and_damage_reads_as_damaged() {
        let dir = tempfile::tempdir().unwrap();
        let store = VerdictStore::new(dir.path());
        assert_eq!(store.read(), VerdictRead::Absent);
        let verdict = json!({"verdict": "up", "incarnation": "1-2"});
        store.write(&verdict).unwrap();
        assert_eq!(store.read(), VerdictRead::Present(verdict));
        assert!(!dir.path().join(format!("{VERDICT_FILE}.tmp")).exists());
        std::fs::write(store.path(), b"{\"verdict\":").unwrap();
        assert!(matches!(store.read(), VerdictRead::Damaged { .. }));
        std::fs::write(store.path(), b"{\"verdict\":\"up\"}").unwrap();
        assert!(matches!(store.read(), VerdictRead::Damaged { .. }));
    }
}
