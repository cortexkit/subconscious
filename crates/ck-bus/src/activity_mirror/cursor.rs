//! Durable activity position. A damaged file is treated as a fresh installation,
//! never as sequence zero, so startup cannot accidentally backfill the ledger.

use cortexkit_lease::durable_replace;
use std::{
    fs, io,
    path::{Path, PathBuf},
};
use subc_client_rs::ActivityCursor;

pub const CURSOR_FILE: &str = "activity_cursor.json";

#[derive(Debug, PartialEq, Eq)]
pub enum CursorRead {
    Absent,
    Present(ActivityCursor),
    Damaged(String),
}

/// Checkpoint storage can fail independently of publishing; keep that boundary
/// injectable so replay after a failed write can be tested without a bus outage.
pub trait Checkpoints: Send + Sync {
    fn path(&self) -> &Path;
    fn remove_stale_tmp(&self);
    fn read(&self) -> CursorRead;
    fn write(&self, cursor: &ActivityCursor) -> io::Result<()>;
}

#[derive(Clone)]
pub struct CursorStore {
    path: PathBuf,
}

impl CursorStore {
    pub fn new(root: &Path) -> Self {
        Self {
            path: root.join(CURSOR_FILE),
        }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn remove_stale_tmp(&self) {
        let _ = fs::remove_file(self.tmp_path());
    }
    pub fn read(&self) -> CursorRead {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return CursorRead::Absent,
            Err(err) => return CursorRead::Damaged(err.to_string()),
        };
        match serde_json::from_slice::<ActivityCursor>(&bytes) {
            Ok(cursor) if !cursor.daemon_incarnation.is_empty() => CursorRead::Present(cursor),
            Ok(_) => CursorRead::Damaged("empty daemon incarnation".into()),
            Err(err) => CursorRead::Damaged(err.to_string()),
        }
    }
    pub fn write(&self, cursor: &ActivityCursor) -> io::Result<()> {
        let bytes = serde_json::to_vec(cursor).map_err(io::Error::other)?;
        let tmp = self.tmp_path();
        fs::write(&tmp, bytes)?;
        durable_replace(&tmp, &self.path)
    }
    fn tmp_path(&self) -> PathBuf {
        let mut name = self.path.file_name().unwrap_or_default().to_os_string();
        name.push(".tmp");
        self.path.with_file_name(name)
    }
}

impl Checkpoints for CursorStore {
    fn path(&self) -> &Path {
        CursorStore::path(self)
    }
    fn remove_stale_tmp(&self) {
        CursorStore::remove_stale_tmp(self);
    }
    fn read(&self) -> CursorRead {
        CursorStore::read(self)
    }
    fn write(&self, cursor: &ActivityCursor) -> io::Result<()> {
        CursorStore::write(self, cursor)
    }
}
