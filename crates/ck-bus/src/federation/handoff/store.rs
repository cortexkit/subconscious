//! Durable send retry bytes, sender epoch and receive dedupe. Store only metadata
//! about quarantined frames, never the rejected ciphertext.

use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use cortexkit_bus_naming::federation::BoxId;
use rand_core::{OsRng, TryRngCore as _};
use rusqlite::{params, Connection, OptionalExtension as _};
use sha2::{Digest, Sha256};

use super::purge::Generation;

// Retain receipt identities beyond every hand-off stream's seven-day max age.
const RECEIPT_RETENTION_SECS: i64 = 8 * 24 * 60 * 60;

pub(super) struct Store {
    db: Connection,
    epoch: u64,
}

pub(super) struct Source {
    pub(super) sequence: u64,
    hash: [u8; 32],
}

impl Source {
    pub(super) fn new(sequence: u64, subject: &str, id: Option<&str>) -> Self {
        let mut hash = Sha256::new();
        hash.update((subject.len() as u64).to_be_bytes());
        hash.update(subject.as_bytes());
        hash.update([u8::from(id.is_some())]);
        if let Some(id) = id {
            hash.update(id.as_bytes());
        }
        Self {
            sequence,
            hash: hash.finalize().into(),
        }
    }
}

impl Store {
    pub(super) fn open(path: &Path) -> rusqlite::Result<Self> {
        let mut db = Connection::open(path)?;
        db.execute_batch(
            "PRAGMA synchronous = FULL;
             CREATE TABLE IF NOT EXISTS sender_epoch (
                 singleton INTEGER PRIMARY KEY CHECK (singleton = 1), epoch BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS sent (
                 peer TEXT PRIMARY KEY, sequence BLOB NOT NULL
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS sealed (
                 source BLOB NOT NULL, hash BLOB NOT NULL, blob BLOB NOT NULL,
                 PRIMARY KEY(source, hash)
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS received (
                 peer TEXT NOT NULL, epoch BLOB NOT NULL, sequence BLOB NOT NULL,
                 stored_at INTEGER NOT NULL, PRIMARY KEY(peer, epoch, sequence)
             ) WITHOUT ROWID;
             CREATE INDEX IF NOT EXISTS received_by_age ON received(stored_at);
             CREATE TABLE IF NOT EXISTS receive_watermarks (
                 peer TEXT NOT NULL, epoch BLOB NOT NULL, sequence BLOB NOT NULL,
                 PRIMARY KEY(peer, epoch)
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS quarantined (
                 source BLOB NOT NULL, hash BLOB NOT NULL, subject TEXT NOT NULL,
                 origin TEXT, reason TEXT NOT NULL, size INTEGER NOT NULL, digest BLOB NOT NULL,
                 PRIMARY KEY(source, hash)
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS hub_purge (
                 singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                 pin BLOB NOT NULL, floor BLOB NOT NULL, finished BLOB NOT NULL,
                 hub_sequence BLOB NOT NULL, cut BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS hub_purge_acked (
                 sequence BLOB PRIMARY KEY, hub_sequence BLOB NOT NULL,
                 verified INTEGER NOT NULL
             ) WITHOUT ROWID;",
        )?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let saved: Option<[u8; 8]> = tx
            .query_row(
                "SELECT epoch FROM sender_epoch WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let epoch = match saved.map(u64::from_be_bytes) {
            Some(0) => return Err(rusqlite::Error::InvalidQuery),
            Some(epoch) => epoch,
            None => {
                let epoch = loop {
                    let n = OsRng.try_next_u64().map_err(|error| {
                        rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                            error.to_string(),
                        )))
                    })?;
                    if n != 0 {
                        break n;
                    }
                };
                tx.execute(
                    "INSERT INTO sender_epoch VALUES (1, ?)",
                    [epoch.to_be_bytes()],
                )?;
                epoch
            }
        };
        tx.commit()?;
        Ok(Self { db, epoch })
    }

    pub(super) fn epoch(&self) -> u64 {
        self.epoch
    }

    pub(super) fn set_purge_generation(&mut self, generation: Generation) -> rusqlite::Result<u64> {
        let tx = self.db.transaction()?;
        let current = tx
            .query_row(
                "SELECT pin, floor, cut FROM hub_purge WHERE singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, [u8; 32]>(0)?,
                        row.get::<_, [u8; 8]>(1)?,
                        row.get::<_, [u8; 8]>(2)?,
                    ))
                },
            )
            .optional()?;
        let floor = generation.floor.to_be_bytes();
        if let Some((pin, old_floor, cut)) = current {
            if generation.pin == pin && floor == old_floor {
                return Ok(u64::from_be_bytes(cut));
            }
        }
        tx.execute("DELETE FROM hub_purge_acked", [])?;
        tx.execute(
            "INSERT OR REPLACE INTO hub_purge VALUES (1, ?1, ?2, ?2, ?3, ?3)",
            params![generation.pin, floor, 0_u64.to_be_bytes()],
        )?;
        tx.commit()?;
        Ok(0)
    }

    /// Advance only through a contiguous prefix of journaled ACKs. An ACK that
    /// wasn't journaled or a deferred frame that expired leaves a gap; max-age
    /// remains the bound rather than purging past either unconfirmed sequence.
    pub(super) fn confirm_purge_receipt(
        &mut self,
        generation: Generation,
        local_sequence: u64,
        hub_sequence: u64,
        verified: bool,
    ) -> rusqlite::Result<Option<u64>> {
        let tx = self.db.transaction()?;
        let state = tx
            .query_row(
                "SELECT finished, hub_sequence, cut FROM hub_purge WHERE pin = ? AND floor = ?",
                params![generation.pin, generation.floor.to_be_bytes()],
                |row| {
                    Ok((
                        u64::from_be_bytes(row.get(0)?),
                        u64::from_be_bytes(row.get(1)?),
                        u64::from_be_bytes(row.get(2)?),
                    ))
                },
            )
            .optional()?;
        let Some((mut finished, mut last, old_cut)) = state else {
            return Ok(None);
        };
        if local_sequence <= finished {
            return Ok(None);
        }
        tx.execute(
            "INSERT OR IGNORE INTO hub_purge_acked VALUES (?, ?, ?)",
            params![
                local_sequence.to_be_bytes(),
                hub_sequence.to_be_bytes(),
                verified
            ],
        )?;
        let mut cut = old_cut;
        if finished.checked_add(1) == Some(local_sequence) {
            let mut statement = tx.prepare_cached(
                "SELECT sequence, hub_sequence, verified FROM hub_purge_acked ORDER BY sequence",
            )?;
            let mut pending = statement.query([])?;
            while let Some(row) = pending.next()? {
                let local = u64::from_be_bytes(row.get(0)?);
                let hub = u64::from_be_bytes(row.get(1)?);
                if finished.checked_add(1) != Some(local) || hub <= last {
                    break;
                }
                finished = local;
                last = hub;
                // Quarantined frames are settled but only verified frames set a cut.
                if row.get::<_, bool>(2)? {
                    cut = hub.checked_add(1).unwrap_or(cut);
                }
            }
        }
        tx.execute(
            "DELETE FROM hub_purge_acked WHERE sequence <= ?",
            [finished.to_be_bytes()],
        )?;
        tx.execute(
            "UPDATE hub_purge SET finished = ?, hub_sequence = ?, cut = ? WHERE singleton = 1",
            params![
                finished.to_be_bytes(),
                last.to_be_bytes(),
                cut.to_be_bytes()
            ],
        )?;
        tx.commit()?;
        Ok((cut > old_cut).then_some(cut))
    }

    pub(super) fn sealed(&self, source: &Source) -> rusqlite::Result<Option<Vec<u8>>> {
        self.db
            .query_row(
                "SELECT blob FROM sealed WHERE source = ? AND hash = ?",
                params![source.sequence.to_be_bytes(), source.hash],
                |row| row.get(0),
            )
            .optional()
    }

    pub(super) fn next(&self, peer: &BoxId) -> rusqlite::Result<u64> {
        let value: Option<[u8; 8]> = self
            .db
            .query_row(
                "SELECT sequence FROM sent WHERE peer = ?",
                [peer.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        value
            .map(u64::from_be_bytes)
            .unwrap_or_default()
            .checked_add(1)
            .ok_or(rusqlite::Error::InvalidQuery)
    }

    pub(super) fn recipients(&self) -> rusqlite::Result<Vec<BoxId>> {
        let mut statement = self.db.prepare("SELECT peer FROM sent")?;
        let rows = statement.query_map([], |row| {
            BoxId::parse(&row.get::<_, String>(0)?).map_err(|_| rusqlite::Error::InvalidQuery)
        })?;
        rows.collect()
    }

    pub(super) fn save_seal(
        &mut self,
        source: &Source,
        peer: &BoxId,
        sequence: u64,
        blob: &[u8],
    ) -> rusqlite::Result<()> {
        let tx = self.db.transaction()?;
        tx.execute(
            "INSERT INTO sealed VALUES (?, ?, ?)",
            params![source.sequence.to_be_bytes(), source.hash, blob],
        )?;
        tx.execute("INSERT INTO sent VALUES (?, ?) ON CONFLICT(peer) DO UPDATE SET sequence = excluded.sequence",
            params![peer.as_str(), sequence.to_be_bytes()])?;
        tx.commit()
    }

    pub(super) fn forget_seal(&self, source: &Source) -> rusqlite::Result<()> {
        self.db.execute(
            "DELETE FROM sealed WHERE source = ? AND hash = ?",
            params![source.sequence.to_be_bytes(), source.hash],
        )?;
        Ok(())
    }

    pub(super) fn seen(&self, peer: &BoxId, epoch: u64, sequence: u64) -> rusqlite::Result<bool> {
        self.db
            .query_row(
                "SELECT 1 FROM received WHERE peer = ? AND epoch = ? AND sequence = ?",
                params![peer.as_str(), epoch.to_be_bytes(), sequence.to_be_bytes()],
                |_| Ok(()),
            )
            .optional()
            .map(|v| v.is_some())
    }

    pub(super) fn received(
        &mut self,
        peer: &BoxId,
        epoch: u64,
        sequence: u64,
    ) -> rusqlite::Result<Option<u64>> {
        let tx = self.db.transaction()?;
        let high: Option<[u8; 8]> = tx
            .query_row(
                "SELECT sequence FROM receive_watermarks WHERE peer = ? AND epoch = ?",
                params![peer.as_str(), epoch.to_be_bytes()],
                |row| row.get(0),
            )
            .optional()?;
        let expected = high
            .map(u64::from_be_bytes)
            .unwrap_or_default()
            .checked_add(1);
        tx.execute(
            "INSERT INTO received VALUES (?, ?, ?, ?)",
            params![
                peer.as_str(),
                epoch.to_be_bytes(),
                sequence.to_be_bytes(),
                unix_secs()
            ],
        )?;
        // Keep the high-water mark only for gap reports, even after quiet periods
        // have aged all receipts out of the dedupe set.
        tx.execute(
            "INSERT INTO receive_watermarks VALUES (?, ?, ?)
             ON CONFLICT(peer, epoch) DO UPDATE SET sequence = excluded.sequence
             WHERE receive_watermarks.sequence < excluded.sequence",
            params![peer.as_str(), epoch.to_be_bytes(), sequence.to_be_bytes()],
        )?;
        tx.commit()?;
        Ok(expected.filter(|expected| sequence > *expected))
    }

    pub(super) fn prune(&self, now: i64) -> rusqlite::Result<()> {
        self.db.execute(
            "DELETE FROM received WHERE stored_at < ?",
            [now.saturating_sub(RECEIPT_RETENTION_SECS)],
        )?;
        Ok(())
    }

    pub(super) fn quarantine(
        &self,
        source: &Source,
        subject: &str,
        origin: Option<&BoxId>,
        reason: &str,
        blob: &[u8],
    ) -> rusqlite::Result<bool> {
        let digest: [u8; 32] = Sha256::digest(blob).into();
        Ok(self.db.execute(
            "INSERT OR IGNORE INTO quarantined VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                source.sequence.to_be_bytes(),
                source.hash,
                subject,
                origin.map(BoxId::as_str),
                reason,
                blob.len(),
                digest
            ],
        )? == 1)
    }

    pub(super) fn quarantined(&self, source: &Source) -> rusqlite::Result<bool> {
        self.db
            .query_row(
                "SELECT 1 FROM quarantined WHERE source = ? AND hash = ?",
                params![source.sequence.to_be_bytes(), source.hash],
                |_| Ok(()),
            )
            .optional()
            .map(|v| v.is_some())
    }
}

pub(super) fn unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
