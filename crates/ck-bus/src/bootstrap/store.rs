//! The two durable shapes bootstrap owns: `account.json` and `own_users.json`.
//!
//! Every write is an atomic durable replacement: a sibling `*.tmp` in the same
//! directory, fsynced, renamed over the target, then the directory fsynced. A file that
//! fails to parse is damaged, and damage is never repaired by rewriting: `account.json`
//! fails closed, and a damaged `own_users.json` is left untouched and named.
//!
//! Neither shape holds key material. `account.json` keeps only the PUBLIC half of the
//! box account's identity key; its seed existed only in the memory of the process that
//! created the account.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use serde_json::{json, Value};
use subc_protocol::MachineId;

pub const ACCOUNT_FILE: &str = "account.json";
pub const OWN_USERS_FILE: &str = "own_users.json";

/// What `account.json` names: the machine id and `{acct}` this store last served, and
/// the box account's identity public key (`A...`), which is the account's id on the
/// server and never changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRecord {
    pub machine_id: String,
    pub acct: String,
    pub account_public: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountFile {
    /// No file. Absence is neutral: the server may still hold the account.
    Absent,
    Present(AccountRecord),
    /// Unreadable, unparsable or inconsistent; the reason names what is wrong.
    Damaged {
        path: PathBuf,
        reason: String,
    },
}

/// ck-bus's own users, as public keys only. `pending_revocation` carries an earlier
/// incarnation's box users until a claims update revoking them is read back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OwnUsers {
    pub box_users: Vec<String>,
    pub system_users: Vec<String>,
    pub pending_revocation: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnUsersFile {
    Absent,
    Present(OwnUsers),
    Damaged { path: PathBuf, reason: String },
}

/// The store root with bootstrap's two files.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn account_path(&self) -> PathBuf {
        self.root.join(ACCOUNT_FILE)
    }

    pub fn own_users_path(&self) -> PathBuf {
        self.root.join(OWN_USERS_FILE)
    }

    /// Removes a stale `*.tmp` left by a write that died before its rename. It was
    /// never the file of record, so removing it loses nothing.
    pub fn remove_stale_tmp(&self) {
        for name in [ACCOUNT_FILE, OWN_USERS_FILE] {
            let _ = fs::remove_file(self.root.join(format!("{name}.tmp")));
        }
    }

    pub fn read_account(&self) -> AccountFile {
        let path = self.account_path();
        let value = match read_json(&path) {
            Ok(None) => return AccountFile::Absent,
            Ok(Some(value)) => value,
            Err(reason) => return AccountFile::Damaged { path, reason },
        };
        match parse_account(&value) {
            Ok(record) => AccountFile::Present(record),
            Err(reason) => AccountFile::Damaged { path, reason },
        }
    }

    pub fn write_account(&self, record: &AccountRecord) -> io::Result<()> {
        write_atomic(
            &self.account_path(),
            &json!({
                "machine_id": record.machine_id,
                "acct": record.acct,
                "account_public": record.account_public,
            }),
        )
    }

    pub fn read_own_users(&self) -> OwnUsersFile {
        let path = self.own_users_path();
        let value = match read_json(&path) {
            Ok(None) => return OwnUsersFile::Absent,
            Ok(Some(value)) => value,
            Err(reason) => return OwnUsersFile::Damaged { path, reason },
        };
        let list = |name: &str| -> Result<Vec<String>, String> {
            value
                .get(name)
                .and_then(Value::as_array)
                .ok_or_else(|| format!("{name} is not a list"))?
                .iter()
                .map(|entry| {
                    entry
                        .as_str()
                        .filter(|key| key.starts_with('U'))
                        .map(str::to_string)
                        .ok_or_else(|| format!("{name} holds a value that is not a user key"))
                })
                .collect()
        };
        let parsed = (|| {
            Ok::<_, String>(OwnUsers {
                box_users: list("box")?,
                system_users: list("system")?,
                pending_revocation: list("pending_revocation")?,
            })
        })();
        match parsed {
            Ok(users) => OwnUsersFile::Present(users),
            Err(reason) => OwnUsersFile::Damaged { path, reason },
        }
    }

    pub fn write_own_users(&self, users: &OwnUsers) -> io::Result<()> {
        write_atomic(
            &self.own_users_path(),
            &json!({
                "box": users.box_users,
                "system": users.system_users,
                "pending_revocation": users.pending_revocation,
            }),
        )
    }
}

fn parse_account(value: &Value) -> Result<AccountRecord, String> {
    let field = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("{name} is missing or not a string"))
    };
    let record = AccountRecord {
        machine_id: field("machine_id")?,
        acct: field("acct")?,
        account_public: field("account_public")?,
    };
    MachineId::parse(&record.machine_id)
        .map_err(|error| format!("machine_id does not parse: {error}"))?;
    if record.acct != format!("box_{}", record.machine_id) {
        return Err(format!(
            "acct {} is not box_ plus machine_id {}",
            record.acct, record.machine_id
        ));
    }
    if !record.account_public.starts_with('A')
        || nkeys::KeyPair::from_public_key(&record.account_public).is_err()
    {
        return Err("account_public is not an account public key".to_string());
    }
    Ok(record)
}

/// `Ok(None)` for a missing file; `Err` names why a present file is damaged.
fn read_json(path: &Path) -> Result<Option<Value>, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("unreadable: {error}")),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("not JSON: {error}"))
}

fn write_atomic(path: &Path, value: &Value) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("store file has no parent directory"))?;
    fs::create_dir_all(dir)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::other("store file has no name"))?;
    let tmp = dir.join(format!("{}.tmp", file_name.to_string_lossy()));
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(value).map_err(io::Error::other)?)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    sync_dir(dir)
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
