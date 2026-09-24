//! The durable progress of each in-flight revocation:
//! `revocation_progress/{module_id}.g{generation}.e{epoch}.json`, one file per revoked
//! (module, generation, epoch).
//!
//! A record carries `highest_completed_step` (0 to 3), the user public key, its
//! `user_jwt_id`, and the kick targets (the server id and client id of each live
//! connection of the user). It is written with step 0 and its inputs before step (1),
//! replaced after each step commits, and removed after step (3). Every write is an atomic
//! durable replacement: a sibling `*.tmp`, fsynced, renamed over the target, then the
//! directory fsynced. The identity comes from the file name, never the body, so a record
//! whose body is damaged still names the revocation it belongs to.

use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use cortexkit_bus_naming::AccountNames;
use serde_json::{json, Value};

pub const PROGRESS_DIR: &str = "revocation_progress";

/// The last step is the kick; a record is removed once it completes.
pub const LAST_STEP: u8 = 3;

/// Which revocation a record belongs to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Identity {
    pub module_id: String,
    pub spawn_generation: u64,
    pub credential_epoch: u64,
}

impl Identity {
    pub fn file_name(&self) -> String {
        format!(
            "{}.g{}.e{}.json",
            self.module_id, self.spawn_generation, self.credential_epoch
        )
    }

    /// Reads the identity out of a record's file name. `None` for a name outside the
    /// grammar, including a module id the naming crate refuses as a census key.
    pub fn parse_file_name(name: &str) -> Option<Self> {
        let stem = name.strip_suffix(".json")?;
        let (rest, epoch) = stem.rsplit_once(".e")?;
        let (module_id, generation) = rest.rsplit_once(".g")?;
        let parse = |digits: &str| {
            (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| digits.parse::<u64>().ok())
                .flatten()
        };
        let identity = Self {
            module_id: module_id.to_string(),
            spawn_generation: parse(generation)?,
            credential_epoch: parse(epoch)?,
        };
        AccountNames::census_key(&identity.module_id).ok()?;
        Some(identity)
    }
}

/// One live connection of a user, as the kick addresses it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KickTarget {
    pub server_id: String,
    pub client_id: u64,
}

/// One readable progress record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub identity: Identity,
    pub highest_completed_step: u8,
    pub user_public: String,
    pub user_jwt_id: String,
    pub kick: BTreeSet<KickTarget>,
}

impl Record {
    fn to_json(&self) -> Value {
        json!({
            "module_id": self.identity.module_id,
            "spawn_generation": self.identity.spawn_generation,
            "credential_epoch": self.identity.credential_epoch,
            "highest_completed_step": self.highest_completed_step,
            "user_public": self.user_public,
            "user_jwt_id": self.user_jwt_id,
            "kick": self
                .kick
                .iter()
                .map(|target| json!({"server_id": target.server_id, "client_id": target.client_id}))
                .collect::<Vec<_>>(),
        })
    }

    fn parse(identity: Identity, bytes: &[u8]) -> Result<Self, String> {
        let value: Value =
            serde_json::from_slice(bytes).map_err(|error| format!("not JSON: {error}"))?;
        let step = value["highest_completed_step"]
            .as_u64()
            .filter(|step| *step <= u64::from(LAST_STEP))
            .ok_or("highest_completed_step is missing or outside 0..=3")?;
        let user_public = value["user_public"]
            .as_str()
            .filter(|key| key.starts_with('U') && nkeys::KeyPair::from_public_key(key).is_ok())
            .ok_or("user_public is missing or not a user public key")?
            .to_string();
        let user_jwt_id = value["user_jwt_id"]
            .as_str()
            .filter(|jti| !jti.is_empty())
            .ok_or("user_jwt_id is missing or empty")?
            .to_string();
        let kick = value["kick"]
            .as_array()
            .ok_or("kick is not a list")?
            .iter()
            .map(|target| {
                Some(KickTarget {
                    server_id: target["server_id"].as_str()?.to_string(),
                    client_id: target["client_id"].as_u64()?,
                })
            })
            .collect::<Option<BTreeSet<_>>>()
            .ok_or("kick holds an entry without server_id and client_id")?;
        Ok(Self {
            identity,
            highest_completed_step: step as u8,
            user_public,
            user_jwt_id,
            kick,
        })
    }
}

/// What one record file holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Present(Record),
    /// Unreadable or unparsable. Its key, jwt id and kick targets are lost; only the
    /// identity from the file name survives.
    Damaged {
        identity: Identity,
        path: PathBuf,
        reason: String,
    },
}

/// The `revocation_progress` directory under the store root.
#[derive(Debug, Clone)]
pub struct ProgressStore {
    dir: PathBuf,
}

impl ProgressStore {
    pub fn new(store_root: &Path) -> Self {
        Self {
            dir: store_root.join(PROGRESS_DIR),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path(&self, identity: &Identity) -> PathBuf {
        self.dir.join(identity.file_name())
    }

    /// Removes each `*.tmp` a write left behind when it died before its rename. It was
    /// never a record, so nothing is lost.
    pub fn remove_stale_tmp(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            if entry.file_name().to_string_lossy().ends_with(".tmp") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    pub fn read(&self, identity: &Identity) -> Option<Entry> {
        let path = self.path(identity);
        match fs::read(&path) {
            Ok(bytes) => Some(match Record::parse(identity.clone(), &bytes) {
                Ok(record) => Entry::Present(record),
                Err(reason) => Entry::Damaged {
                    identity: identity.clone(),
                    path,
                    reason,
                },
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => Some(Entry::Damaged {
                identity: identity.clone(),
                path,
                reason: format!("unreadable: {error}"),
            }),
        }
    }

    /// Every record in the directory, in file-name order. A file whose name is outside
    /// the grammar names no revocation and is skipped; a missing directory holds none.
    pub fn list(&self) -> io::Result<Vec<Entry>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        Ok(names
            .iter()
            .filter_map(|name| Identity::parse_file_name(name))
            .filter_map(|identity| self.read(&identity))
            .collect())
    }

    pub fn write(&self, record: &Record) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let path = self.path(&record.identity);
        let tmp = self
            .dir
            .join(format!("{}.tmp", record.identity.file_name()));
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            file.write_all(
                &serde_json::to_vec_pretty(&record.to_json()).map_err(io::Error::other)?,
            )?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        sync_dir(&self.dir)
    }

    /// Removes a record once its revocation is complete, or once recovery has shown
    /// there is nothing left to do. An absent record is already cleared.
    pub fn clear(&self, identity: &Identity) -> io::Result<()> {
        match fs::remove_file(self.path(identity)) {
            Ok(()) => sync_dir(&self.dir),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
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
    use super::*;

    fn record() -> Record {
        Record {
            identity: Identity {
                module_id: "participant".to_string(),
                spawn_generation: 4,
                credential_epoch: 2,
            },
            highest_completed_step: 1,
            user_public: nkeys::KeyPair::new_user().public_key(),
            user_jwt_id: "JTI".to_string(),
            kick: BTreeSet::from([KickTarget {
                server_id: "NSERVER".to_string(),
                client_id: 9,
            }]),
        }
    }

    #[test]
    fn a_record_round_trips_under_its_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProgressStore::new(dir.path());
        let record = record();
        store.write(&record).unwrap();
        assert!(store.dir().join("participant.g4.e2.json").is_file());
        assert_eq!(store.list().unwrap(), vec![Entry::Present(record.clone())]);
        store.clear(&record.identity).unwrap();
        assert_eq!(store.list().unwrap(), vec![]);
        store
            .clear(&record.identity)
            .expect("clearing twice is fine");
    }

    #[test]
    fn the_identity_comes_from_the_file_name_and_a_bad_body_is_damage() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProgressStore::new(dir.path());
        let mut record = record();
        store.write(&record).unwrap();
        let path = store.path(&record.identity);
        // A body naming another identity changes nothing: the name wins.
        let mut body: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        body["module_id"] = json!("someone-else");
        fs::write(&path, serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(store.list().unwrap(), vec![Entry::Present(record.clone())]);

        fs::write(&path, b"{\"highest_completed_step\": 7").unwrap();
        let Entry::Damaged { identity, .. } = store.read(&record.identity).unwrap() else {
            panic!("a short file is damaged");
        };
        assert_eq!(identity, record.identity);
        record.highest_completed_step = 9;
        fs::write(&path, serde_json::to_vec(&record.to_json()).unwrap()).unwrap();
        assert!(matches!(
            store.read(&record.identity),
            Some(Entry::Damaged { .. })
        ));
    }

    #[test]
    fn names_outside_the_grammar_are_not_records() {
        for name in [
            "participant.g4.json",
            "participant.gx.e1.json",
            "a.b.g1.e1.json",
            "participant.g1.e1.json.tmp",
            "*.g1.e1.json",
        ] {
            assert_eq!(Identity::parse_file_name(name), None, "{name}");
        }
        assert_eq!(
            Identity::parse_file_name("participant.g12.e0.json"),
            Some(Identity {
                module_id: "participant".to_string(),
                spawn_generation: 12,
                credential_epoch: 0,
            })
        );
    }

    #[test]
    fn a_stale_tmp_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProgressStore::new(dir.path());
        fs::create_dir_all(store.dir()).unwrap();
        let tmp = store.dir().join("participant.g1.e0.json.tmp");
        fs::write(&tmp, b"partial").unwrap();
        store.remove_stale_tmp();
        assert!(!tmp.exists());
    }
}
