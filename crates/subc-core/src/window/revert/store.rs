use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::{
    files::{self, Kind, Reading},
    journal::{
        self, content, durable,
        paths::{self, error_at},
        Floor,
    },
    placement,
};

const SIDECARS: [&str; 3] = ["-wal", "-shm", "-journal"];

pub(super) struct Store<'a> {
    pub target: &'a Path,
    pub directory: &'a Path,
    pub index: usize,
    pub sha: &'a str,
    pub floors: &'a [Floor],
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Member {
    name: PathBuf,
    sha256: String,
    mode: Option<u32>,
}

type Files = BTreeMap<PathBuf, Reading>;

struct Attempt {
    dir: PathBuf,
    members: Vec<Member>,
}

impl Store<'_> {
    fn tree(&self) -> &Path {
        self.target.parent().expect("validated store path")
    }

    fn saved(&self) -> PathBuf {
        placement::saved(self.directory, self.index)
    }

    fn attempts(&self) -> PathBuf {
        self.directory.join("aside").join(self.index.to_string())
    }

    pub(super) fn preflight(&self) -> Result<(), String> {
        if self
            .target
            .file_name()
            .is_some_and(|name| name == "format-floor.json")
        {
            return Err(error_at(
                self.target,
                "store name collides with a format-floor file",
            ));
        }
        Kind::File.snapshot(self.target)?;
        let snapshot = self.saved().join("snapshot");
        files::check_parent(&snapshot)?;
        Kind::File.verify(&snapshot, self.sha)?;
        for floor in self.floors {
            let copy = self.saved().join("floors").join(&floor.path);
            files::check_parent(&copy)?;
            Kind::File.verify(&copy, &floor.sha256)?;
        }
        Ok(())
    }

    fn sidecars(&self) -> impl Iterator<Item = PathBuf> + '_ {
        SIDECARS
            .iter()
            .map(|suffix| files::suffix(self.target, suffix))
    }

    fn live(&self) -> Result<Vec<Member>, String> {
        files::check_parent(self.target)?;
        self.sidecars()
            .chain([self.target.to_owned()])
            .chain(
                content::floor_paths(self.tree())?
                    .into_iter()
                    .map(|name| self.tree().join(name)),
            )
            .map(|path| {
                let Reading { digest, mode } = Kind::File.snapshot(&path)?;
                Ok(digest.map(|sha256| Member {
                    name: self.relative(&path).to_owned(),
                    sha256,
                    mode,
                }))
            })
            .filter_map(Result::transpose)
            .collect()
    }

    fn relative<'a>(&self, path: &'a Path) -> &'a Path {
        path.strip_prefix(self.tree())
            .expect("store member in tree")
    }

    fn placed(&self) -> BTreeMap<PathBuf, &str> {
        std::iter::once((self.relative(self.target).to_owned(), self.sha))
            .chain(
                self.floors
                    .iter()
                    .map(|floor| (floor.path.clone(), floor.sha256.as_str())),
            )
            .collect()
    }

    fn most_recent_attempt(&self) -> Result<(u64, Option<Attempt>), String> {
        let root = self.attempts();
        if !paths::plain_dir(&root)? {
            return Ok((0, None));
        }
        files::check_parent(&root.join("entry"))?;
        let mut last = 0;
        let mut published = None;
        for path in journal::entries(&root)? {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            let n = name
                .parse::<u64>()
                .map_err(|_| error_at(&path, "invalid attempt name"))?;
            if n == 0 || name != n.to_string() || !paths::plain_dir(&path)? {
                return Err(error_at(&path, "invalid attempt directory"));
            }
            last = last.max(n);
            if paths::info(&path.join("members"))?.is_some()
                && published.as_ref().is_none_or(|(old, _)| n > *old)
            {
                published = Some((n, path));
            }
        }
        let attempt = published
            .map(|(_, dir)| self.read_attempt(dir))
            .transpose()?;
        Ok((last, attempt))
    }

    fn read_attempt(&self, dir: PathBuf) -> Result<Attempt, String> {
        let manifest = dir.join("members");
        let file = subc_os::open_regular_nofollow(&manifest)
            .map_err(|error| error_at(&manifest, error))?;
        let members: Vec<Member> =
            serde_json::from_reader(file).map_err(|error| error_at(&manifest, error))?;
        let mut seen = Vec::<&Path>::new();
        for member in &members {
            let path = &member.name;
            let is_floor = path
                .file_name()
                .is_some_and(|name| name == "format-floor.json")
                && path
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)));
            let is_special = path == self.relative(self.target)
                || self
                    .sidecars()
                    .any(|sidecar| path == self.relative(&sidecar));
            if (!is_floor && !is_special)
                || seen.iter().any(|earlier| paths::same_path(earlier, path))
                || member.sha256.len() != 64
                || !member.sha256.bytes().all(|b| b.is_ascii_hexdigit())
                || member.mode.is_some_and(|mode| mode > 0o7777)
            {
                return Err(error_at(&manifest, "invalid or duplicate store member"));
            }
            seen.push(path);
        }
        Ok(Attempt { dir, members })
    }

    fn publish(&self, n: u64, members: Vec<Member>) -> Result<Attempt, String> {
        let dir = self.attempts().join(n.to_string());
        durable::ensure_dir(self.directory, &dir)?;
        let partial = dir.join("members.partial");
        let mut file = crate::setup::install_journal::open_journal(&partial, true)
            .map_err(|error| error_at(&partial, error))?;
        serde_json::to_writer(&mut file, &members).map_err(|error| error_at(&partial, error))?;
        file.write_all(b"\n")
            .map_err(|error| error_at(&partial, error))?;
        durable::file(&partial, &file)?;
        drop(file);
        files::move_new(&partial, &dir.join("members"))?;
        Ok(Attempt { dir, members })
    }

    pub(super) fn undo(
        &self,
        integrity: impl FnOnce(&Path) -> Result<(), String>,
        checkpoint: impl Fn(&str) -> Result<(), String>,
    ) -> Result<(), String> {
        let live = self.live()?;
        let placed = self.placed();
        let (last, previous) = self.most_recent_attempt()?;
        let unfinished = match previous {
            Some(previous) if !previous.rolled_back(&live)? => Some(previous),
            _ => None,
        };
        let exact = |live: &[Member]| {
            live.len() == placed.len()
                && live
                    .iter()
                    .all(|member| placed.get(&member.name).copied() == Some(member.sha256.as_str()))
        };
        let attempt = if let Some(attempt) = unfinished {
            Some(attempt)
        } else if exact(&live) {
            None
        } else {
            Some(
                self.publish(
                    last.checked_add(1)
                        .ok_or("store attempt number exhausted")?,
                    live,
                )?,
            )
        };

        if let Some(attempt) = &attempt {
            self.set_aside(attempt, &placed)?;
            checkpoint("after-store-set-aside")?;
            self.place(&placed, &checkpoint)?;
        }
        checkpoint("before-store-integrity")?;
        let checked = (|| {
            if !exact(&self.live()?) {
                return Err(error_at(
                    self.target,
                    "store or floors changed before integrity_check",
                ));
            }
            integrity(self.target)?;
            if !exact(&self.live()?) {
                return Err(error_at(
                    self.target,
                    "store or floors changed during integrity_check",
                ));
            }
            durable::parent(self.target)
        })();
        match (checked, attempt) {
            (Err(error), Some(attempt)) => match self.rollback(&attempt, &placed) {
                Ok(()) => Err(error),
                Err(rollback) => Err(format!("{error}; store rollback incomplete: {rollback}")),
            },
            (result, _) => result,
        }
    }

    fn recognized(
        &self,
        attempt: &Attempt,
        placed: &BTreeMap<PathBuf, &str>,
    ) -> Result<Files, String> {
        self.live()?
            .into_iter()
            .map(|member| {
                let original = attempt
                    .members
                    .iter()
                    .any(|m| m.name == member.name && m.sha256 == member.sha256);
                let restored = placed.get(&member.name).copied() == Some(member.sha256.as_str());
                if !original && !restored {
                    return Err(error_at(
                        &self.tree().join(&member.name),
                        "not an original or a restored store member",
                    ));
                }
                Ok((
                    member.name,
                    Reading {
                        digest: Some(member.sha256),
                        mode: member.mode,
                    },
                ))
            })
            .collect()
    }

    fn set_aside(&self, attempt: &Attempt, placed: &BTreeMap<PathBuf, &str>) -> Result<(), String> {
        let live = self.recognized(attempt, placed)?;
        durable::parent(&attempt.dir.join("members"))?;
        for member in &attempt.members {
            let target = self.tree().join(&member.name);
            let aside = attempt.artifact(&member.name);
            durable::ensure_dir(&attempt.dir, aside.parent().expect("artifact parent"))?;
            let current = live.get(&member.name).cloned().unwrap_or_default();
            files::require_reading(Kind::File, &target, &current)?;
            files::set_aside(Kind::File, &target, &aside, &member.sha256)?;
        }
        Ok(())
    }

    fn place(
        &self,
        placed: &BTreeMap<PathBuf, &str>,
        checkpoint: impl Fn(&str) -> Result<(), String>,
    ) -> Result<(), String> {
        let saved = self.saved();
        let store = self.relative(self.target);
        for (name, sha) in placed {
            let target = self.tree().join(name);
            let (source, mode) = if name == store {
                (saved.join("snapshot"), cfg!(unix).then_some(0o600))
            } else {
                (saved.join("floors").join(name), None)
            };
            self.ensure_parent(&target)?;
            let current = Kind::File.snapshot(&target)?;
            if current.digest.as_deref().is_some_and(|found| found != *sha) {
                return Err(error_at(&target, "changed before store restore"));
            }
            if current.digest.is_none() || mode.is_some() && current.mode != mode {
                files::check_parent(&source)?;
                files::restore_copy(Kind::File, &source, &target, sha, mode, &current)?;
            }
            durable::parent(&target)?;
            if name != store {
                checkpoint("after-store-floor")?;
            }
        }
        Ok(())
    }

    fn rollback(&self, attempt: &Attempt, placed: &BTreeMap<PathBuf, &str>) -> Result<(), String> {
        let live = self.recognized(attempt, placed)?;
        // Check every archive before removing any placed file. Restoring by copy
        // keeps all attempts, even when a later retry needs a new attempt number.
        for member in &attempt.members {
            let aside = attempt.artifact(&member.name);
            files::check_parent(&aside)?;
            Kind::File.verify(&aside, &member.sha256)?;
        }
        attempt.mark_rollback()?;
        for (name, current) in &live {
            if !attempt.members.iter().any(|member| {
                member.name == *name
                    && current.digest.as_deref() == Some(&member.sha256)
                    && current.mode == member.mode
            }) {
                files::remove_checked(Kind::File, &self.tree().join(name), current)?;
            }
        }
        for member in &attempt.members {
            let target = self.tree().join(&member.name);
            self.ensure_parent(&target)?;
            let reading = Kind::File.snapshot(&target)?;
            if reading.digest.is_none() {
                files::restore_copy(
                    Kind::File,
                    &attempt.artifact(&member.name),
                    &target,
                    &member.sha256,
                    member.mode,
                    &reading,
                )?;
            } else if reading.digest.as_deref() != Some(&member.sha256)
                || reading.mode != member.mode
            {
                return Err(error_at(&target, "changed during store rollback"));
            }
        }
        if !same_bytes(&attempt.members, &self.live()?) {
            return Err(error_at(
                self.target,
                "original store members were not restored",
            ));
        }
        Ok(())
    }

    fn ensure_parent(&self, target: &Path) -> Result<(), String> {
        files::check_parent(self.target)?;
        durable::ensure_dir(self.tree(), target.parent().expect("member parent"))?;
        files::check_parent(target)?;
        Ok(())
    }
}

impl Attempt {
    fn rolled_back(&self, live: &[Member]) -> Result<bool, String> {
        Ok(content::file_info(&self.dir.join("rollback"))?.is_some()
            && same_bytes(&self.members, live))
    }

    fn mark_rollback(&self) -> Result<(), String> {
        let path = self.dir.join("rollback");
        if content::file_info(&path)?.is_none() {
            let stage = self.dir.join("rollback.partial");
            durable::remove_artifact(&stage)?;
            let file = crate::setup::install_journal::open_journal(&stage, true)
                .map_err(|error| error_at(&stage, error))?;
            durable::file(&stage, &file)?;
            drop(file);
            files::move_new(&stage, &path)?;
        }
        durable::parent(&path)
    }

    fn artifact(&self, name: &Path) -> PathBuf {
        // Keep user-chosen store names separate from the attempt's metadata.
        self.dir.join("files").join(name)
    }
}

fn same_bytes(original: &[Member], live: &[Member]) -> bool {
    original.len() == live.len()
        && original.iter().all(|member| {
            live.iter()
                .any(|other| member.name == other.name && member.sha256 == other.sha256)
        })
}

pub(super) fn integrity_check(path: &Path) -> Result<(), String> {
    use rusqlite::{Connection, OpenFlags};
    // `immutable=1` is only used after checking for all recovery sidecars. It
    // checks the placed bytes without writing or creating any files beside them.
    let mut header = [0_u8; 16];
    subc_os::open_regular_nofollow(path)
        .and_then(|mut f| Ok(f.read_exact(&mut header)?))
        .map_err(|error| error_at(path, format!("integrity_check: {error}")))?;
    if &header != b"SQLite format 3\0" {
        return Err(error_at(path, "integrity_check: not a SQLite database"));
    }
    let mut uri = String::from("file:");
    for byte in path
        .to_str()
        .ok_or_else(|| error_at(path, "store path is not UTF-8"))?
        .bytes()
    {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.') {
            uri.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(&mut uri, "%{byte:02X}").expect("write to string");
        }
    }
    uri.push_str("?immutable=1");
    let error_at = |e: rusqlite::Error| error_at(path, format!("integrity_check: {e}"));
    let db = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(error_at)?;
    let mut check = db.prepare("PRAGMA integrity_check").map_err(error_at)?;
    let mut rows = check.query([]).map_err(error_at)?;
    let first = rows
        .next()
        .map_err(error_at)?
        .ok_or_else(|| format!("{}: integrity_check returned no result", path.display()))?;
    let value: String = first.get(0).map_err(error_at)?;
    if value != "ok" || rows.next().map_err(error_at)?.is_some() {
        return Err(format!("{}: integrity_check: {value}", path.display()));
    }
    Ok(())
}
