use std::{
    fs,
    path::{Path, PathBuf},
};

use cortexkit_test_support::ScratchDir;

use super::{host::ModuleReading, *};
#[cfg(unix)]
use crate::stack::host::ProcessReading;
use crate::{
    setup::install_journal::open_journal,
    window::journal::{content, Action, Role, SnapshotTransfer},
};

const NOW: u64 = 1_709_251_199_123;

struct Fake {
    stamped: Option<bool>,
    liveness: Liveness,
    manager: Result<bool, String>,
    daemon: Option<Owner>,
    daemon_path: PathBuf,
    module_state: Option<(u64, bool, bool)>,
    image_proof: bool,
    image_matches: bool,
}

impl Default for Fake {
    fn default() -> Self {
        Self {
            stamped: Some(false),
            liveness: Liveness::Alive,
            manager: Ok(false),
            daemon: None,
            daemon_path: PathBuf::new(),
            module_state: None,
            image_proof: false,
            image_matches: false,
        }
    }
}

impl Host for Fake {
    fn stamped(&mut self) -> Result<Option<bool>, String> {
        Ok(self.stamped)
    }
    fn owner(&self, pid: u32) -> Result<Owner, String> {
        if pid != 10 && pid != 20 {
            Err(format!("pid {pid} is not an ancestor"))
        } else {
            Ok(identity(pid, 123))
        }
    }
    fn liveness(&self, _: &Owner) -> Liveness {
        self.liveness
    }
    fn manager_live(&mut self) -> Result<bool, String> {
        self.manager.clone()
    }
    fn daemon_path(&self) -> Result<PathBuf, String> {
        Ok(self.daemon_path.clone())
    }
    fn daemon(&mut self) -> Result<Option<Owner>, String> {
        Ok(self.daemon.clone())
    }
    fn module(&mut self, id: &str) -> Result<ModuleReading, String> {
        let (generation, live, image_disagrees) =
            self.module_state.ok_or_else(|| format!("no module {id}"))?;
        Ok(ModuleReading {
            generation,
            process: live.then(|| identity(91, 100 + generation)),
            image_disagrees,
        })
    }
    fn has_running_image_proof(&self) -> bool {
        self.image_proof
    }
    fn image_matches(&self, _: &Owner, _: &Path) -> bool {
        self.image_matches
    }
}

struct DiskHost(Option<(u32, Liveness)>);

impl Host for DiskHost {
    fn stamped(&mut self) -> Result<Option<bool>, String> {
        panic!("disk reads must not probe for a stamped install")
    }
    fn owner(&self, _: u32) -> Result<Owner, String> {
        panic!("disk reads must use the recorded owner")
    }
    fn liveness(&self, owner: &Owner) -> Liveness {
        let (pid, reading) = self.0.expect("no recorded owner should be checked");
        assert_eq!(owner.pid, pid);
        reading
    }
    fn manager_live(&mut self) -> Result<bool, String> {
        panic!("disk reads must not query the manager")
    }
    fn daemon_path(&self) -> Result<PathBuf, String> {
        panic!("disk reads must not resolve the daemon path")
    }
    fn daemon(&mut self) -> Result<Option<Owner>, String> {
        panic!("disk reads must not query the daemon")
    }
    fn module(&mut self, _: &str) -> Result<ModuleReading, String> {
        panic!("disk reads must not query module state")
    }
    fn has_running_image_proof(&self) -> bool {
        panic!("disk reads must not check running images")
    }
    fn image_matches(&self, _: &Owner, _: &Path) -> bool {
        panic!("disk reads must not check running images")
    }
}

fn identity(pid: u32, start_time: u64) -> Owner {
    Owner {
        pid,
        start_time,
        executable: Some("owner".into()),
        executable_identity: None,
        boot: cfg!(unix).then(|| "01234567-89ab-cdef-0123-0123456789ab".into()),
    }
}

struct Rig {
    _scratch: ScratchDir,
    windows: Windows,
    base: PathBuf,
    host: Fake,
}

impl Rig {
    fn new(name: &str) -> Self {
        let scratch = ScratchDir::new(name);
        let root = scratch.path().join("install");
        fs::create_dir(&root).unwrap();
        let windows = Windows::at(&root, None).unwrap();
        let base = fs::canonicalize(scratch.path()).unwrap();
        Self {
            _scratch: scratch,
            windows,
            base,
            host: Fake::default(),
        }
    }

    fn begin(&mut self, note: Option<&str>) -> String {
        self.windows
            .begin_with(note, &mut self.host, NOW, || 1, || Ok(()))
            .unwrap()
    }

    fn path(&self, id: &str, path: &str) -> PathBuf {
        self.windows.root().join("windows").join(id).join(path)
    }

    fn file(&self, name: &str, bytes: &str) -> PathBuf {
        let path = self.base.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }

    fn record(&mut self, id: &str, request: RecordRequest) -> Result<usize, WindowError> {
        self.windows.record_with(id, 10, request, &mut self.host)
    }

    fn done(&mut self, id: &str, index: usize) -> Result<(), WindowError> {
        self.windows.done_with(id, 10, index, &mut self.host)
    }

    fn close(&mut self, id: Option<&str>, how: Closing) -> Result<Close, WindowError> {
        self.windows.close(id, how, &mut self.host, |_| Ok(()))
    }

    fn list(&mut self) -> List {
        self.windows.list_with(&mut self.host).unwrap()
    }
}

fn file_request(target: &Path, new: Option<&Path>) -> RecordRequest {
    RecordRequest::File {
        target: target.into(),
        new: new.map(Path::to_owned),
        module: None,
        role: None,
    }
}

fn activation(target: &str) -> RecordRequest {
    RecordRequest::Activation {
        target: target.into(),
    }
}

fn seed_disk_window(rig: &Rig, began_ms: u64) -> String {
    let id = id::candidate(began_ms, 1);
    let dir = journal::dir(rig.windows.root(), &id).unwrap();
    fs::create_dir_all(&dir).unwrap();
    journal::create(&dir, &id, began_ms, None).unwrap();
    id
}

#[test]
fn disk_rows_empty_does_not_probe_or_create_windows() {
    let rig = Rig::new("window-disk-empty");
    assert!(rig.windows.disk_rows_with(&DiskHost(None)).is_empty());
    assert!(!rig.windows.root().join("windows").exists());
}

#[test]
fn disk_rows_preserves_unreadable_newest_and_older_rows() {
    for unreadable_index in [0, 1] {
        let rig = Rig::new("window-disk-unreadable");
        let older = seed_disk_window(&rig, NOW);
        let newer = seed_disk_window(&rig, NOW + 1_000);
        let ids = [newer, older];
        let path = rig.path(&ids[unreadable_index], "journal");
        fs::write(&path, b"").unwrap();

        let rows = rig.windows.disk_rows_with(&DiskHost(None));
        assert_eq!(rows.len(), 2);
        for (index, (row, id)) in rows.into_iter().zip(ids).enumerate() {
            assert_eq!(row.owner_liveness, None);
            if index == unreadable_index {
                assert_eq!(
                    row.row.unwrap_err(),
                    (id, format!("{}: missing begin record", path.display()))
                );
            } else {
                assert_eq!(row.row.unwrap().id, id);
            }
        }
    }
}

#[test]
fn disk_rows_reports_a_scan_failure_as_one_unreadable_row() {
    let rig = Rig::new("window-disk-scan-error");
    let path = rig.windows.root().join("windows");
    fs::write(&path, b"not a directory").unwrap();

    let mut rows = rig.windows.disk_rows_with(&DiskHost(None));
    assert_eq!(rows.len(), 1);
    let row = rows.pop().unwrap();
    assert_eq!(row.owner_liveness, None);
    assert_eq!(
        row.row.unwrap_err(),
        (
            "windows".into(),
            format!("{}: not a plain directory", path.display())
        )
    );
}

#[test]
fn disk_rows_keeps_revert_interrupted_behind_closed_newest() {
    let mut rig = Rig::new("window-disk-reverting");
    let older = rig.begin(None);
    let target = rig.file("target", "old");
    rig.record(&older, file_request(&target, None)).unwrap();
    let newer = seed_disk_window(&rig, NOW + 1_000);
    for (id, pid) in [(&older, 41), (&newer, 42)] {
        let mut locked = Locked::open(rig.windows.root(), id).unwrap();
        locked
            .append(Record::RevertStart {
                owner: identity(pid, 10),
            })
            .unwrap();
        if id == &newer {
            locked.append(Record::Abandoned).unwrap();
        }
    }

    let rows = rig
        .windows
        .disk_rows_with(&DiskHost(Some((41, Liveness::Gone))));
    assert_eq!(rows.len(), 2);
    let closed = rows[0].row.as_ref().unwrap();
    assert_eq!(closed.id, newer);
    assert_eq!(closed.state(|_| unreachable!()), State::Abandoned);
    assert_eq!(rows[0].owner_liveness, None);
    let reverting = rows[1].row.as_ref().unwrap();
    assert_eq!(reverting.id, older);
    assert_eq!(rows[1].owner_liveness, Some(Liveness::Gone));
    assert_eq!(
        reverting.state(|_| rows[1].owner_liveness.unwrap()),
        State::RevertInterrupted
    );
}

#[test]
fn disk_rows_keeps_the_unfinished_step_owners_reading() {
    let mut rig = Rig::new("window-disk-liveness");
    let id = rig.begin(None);
    let target = rig.file("target", "old");
    rig.record(&id, file_request(&target, None)).unwrap();

    for reading in [Liveness::Alive, Liveness::Gone, Liveness::Unreadable] {
        let rows = rig.windows.disk_rows_with(&DiskHost(Some((10, reading))));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].row.as_ref().unwrap().id, id);
        assert_eq!(rows[0].owner_liveness, Some(reading));
    }

    fs::remove_file(target).unwrap();
    rig.done(&id, 1).unwrap();
    let rows = rig.windows.disk_rows_with(&DiskHost(None));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].row.as_ref().unwrap().id, id);
    assert_eq!(rows[0].owner_liveness, None);
}

#[test]
fn disk_rows_returns_all_windows_newest_first() {
    let rig = Rig::new("window-disk-all");
    let mut ids: Vec<_> = (0..21)
        .map(|i| seed_disk_window(&rig, NOW + i * 1_000))
        .collect();
    ids.reverse();

    let rows = rig.windows.disk_rows_with(&DiskHost(None));
    assert_eq!(rows.len(), ids.len());
    for (row, id) in rows.into_iter().zip(ids) {
        assert_eq!(row.row.unwrap().id, id);
        assert_eq!(row.owner_liveness, None);
    }
}

#[test]
fn begin_reserves_valid_id_ignores_crash_temporary_and_retries_collision() {
    let mut rig = Rig::new("window-begin-atomic");
    assert_eq!(
        rig.windows
            .begin_with(Some("bad\n"), &mut rig.host, NOW, || 1, || Ok(()))
            .unwrap_err()
            .exit,
        2
    );
    let crash = rig
        .windows
        .begin_with(
            Some("release"),
            &mut rig.host,
            NOW,
            || 1,
            || Err("crash before rename".into()),
        )
        .unwrap_err();
    assert_eq!(crash.exit, 1);
    let id = id::candidate(NOW, 1);
    assert_eq!(id, "20240229-235959-0001");
    assert!(id::valid(&id));
    assert!(!id::valid("20240230-235959-0001"));
    assert!(!id::valid("20240229-235959-../1"));
    assert!(rig
        .windows
        .snapshot("../installer.journal")
        .unwrap_err()
        .message
        .contains("invalid window id"));
    let old_temp = rig.path(&format!(".tmp-{id}"), "journal");
    assert!(old_temp.is_file());
    assert_eq!(rig.windows.current_with(&mut rig.host).unwrap(), None);
    assert_eq!(rig.begin(None), id);
    assert!(!old_temp.exists());
    rig.close(Some(&id), Closing::Abandon).unwrap();
    let mut attempts = [1, 2].into_iter();
    assert_eq!(
        rig.windows
            .begin_with(
                None,
                &mut rig.host,
                NOW,
                || attempts.next().unwrap(),
                || Ok(())
            )
            .unwrap(),
        id::candidate(NOW, 2)
    );
    assert_eq!(rig.list().windows[0].id, id::candidate(NOW, 2));
}

#[test]
fn installer_blocks_begin_and_window_records_do_not_decode_as_installer_records() {
    use std::io::Write as _;
    let mut rig = Rig::new("window-installer-isolation");
    let manifest = rig.windows.root().join("installer-manifest.json");
    let path = Journal::path(&manifest);
    fs::write(&path, b"unopened").unwrap();
    assert!(rig
        .windows
        .begin_with(None, &mut rig.host, NOW, || 1, || Ok(()))
        .unwrap_err()
        .message
        .contains("installer journal"));
    assert!(!rig.windows.root().join("windows").exists());
    fs::remove_file(&path).unwrap();
    let id = rig.begin(None);
    assert!(!Journal::exists(&manifest).unwrap());
    for record in [
        r#"{"record":"begin","version":1,"id":"abcd","unprotected":true}"#,
        "{\"record\":\"begin\",\"version\":1,\"id\":\"abcd\"}\n{\"record\":\"restored\",\"targets\":[]}",
    ] {
        let mut journal = open_journal(&path, true).unwrap();
        writeln!(journal, "{record}").unwrap();
        drop(journal);
        assert!(Journal::open(&manifest).is_err());
        fs::remove_file(&path).unwrap();
    }
    assert_eq!(
        rig.windows.current_with(&mut rig.host).unwrap().as_deref(),
        Some(id.as_str())
    );
}

#[test]
fn unreadable_window_is_visible_but_refuses_commands_without_fixing_journal() {
    use std::io::Write as _;
    let mut rig = Rig::new("window-required-owner");
    let id = rig.begin(Some("repair me"));
    let target = rig.file("target", "old");
    rig.record(&id, file_request(&target, None)).unwrap();
    let path = rig.path(&id, "journal");
    let bytes = fs::read_to_string(&path).unwrap();
    let mut lines = bytes.lines();
    let first = lines.next().unwrap();
    let mut bad: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    bad["step"].as_object_mut().unwrap().remove("owner");
    let mut journal = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    writeln!(journal, "{first}\n{bad}").unwrap();
    drop(journal);
    let corrupted = fs::read(&path).unwrap();
    let summary = rig.list();
    assert_eq!(summary.windows[0].state, State::Unreadable);
    assert!(summary.human().contains("missing field `owner`"));
    assert!(summary.human().contains(MANUAL));
    for refusal in [
        rig.windows.current_with(&mut rig.host).unwrap_err(),
        rig.windows
            .begin_with(None, &mut rig.host, NOW, || 2, || Ok(()))
            .unwrap_err(),
        rig.windows.revert_candidate(Some(&id)).unwrap_err(),
        rig.record(&id, file_request(&target, None)).unwrap_err(),
        rig.done(&id, 1).unwrap_err(),
        rig.close(None, Closing::Keep).unwrap_err(),
        rig.close(Some(&id), Closing::Abandon).unwrap_err(),
    ] {
        assert_eq!(refusal.exit, 2);
        assert!(
            refusal.message.contains(&id) && refusal.message.contains(MANUAL),
            "{refusal}"
        );
    }
    assert_eq!(fs::read(path).unwrap(), corrupted);
}

#[test]
fn record_checks_copy_space_owner_orphan_and_done_digests() {
    let mut rig = Rig::new("window-copy-proof");
    let id = rig.begin(None);
    let target = rig.file("app", "old payload");
    let source = rig.file("new", "replace with me");
    let request = || file_request(&target, Some(&source));
    assert!(rig
        .windows
        .record_with(&id, 30, request(), &mut rig.host)
        .unwrap_err()
        .message
        .contains("not an ancestor"));
    content::SPACE.with(|slot| slot.set(Some(0)));
    assert!(rig
        .record(&id, request())
        .unwrap_err()
        .message
        .contains("not enough free space"));
    content::SPACE.with(|slot| slot.set(None));
    content::TRUNCATE.with(|flag| flag.set(true));
    assert!(rig
        .record(&id, request())
        .unwrap_err()
        .message
        .contains("does not match source digest"));
    assert_eq!(rig.windows.snapshot(&id).unwrap().record_count, 1);
    assert!(rig.path(&id, "saved/.tmp-1").is_file());
    fs::write(rig.path(&id, "saved/1"), b"orphan").unwrap();
    let hold = Locked::open(rig.windows.root(), &id).unwrap();
    assert!(rig
        .record(&id, request())
        .unwrap_err()
        .message
        .contains("holds the lock"));
    assert_eq!(fs::read(rig.path(&id, "saved/1")).unwrap(), b"orphan");
    drop(hold);
    assert_eq!(rig.record(&id, request()).unwrap(), 1);
    assert_eq!(fs::read(rig.path(&id, "saved/1")).unwrap(), b"old payload");
    assert!(!rig.path(&id, "saved/.tmp-1").exists());
    assert!(rig
        .record(&id, request())
        .unwrap_err()
        .message
        .contains("busy"));
    assert!(rig
        .windows
        .done_with(&id, 20, 1, &mut rig.host)
        .unwrap_err()
        .message
        .contains("belongs to"));
    assert!(rig
        .windows
        .done_with(&id, 30, 1, &mut rig.host)
        .unwrap_err()
        .message
        .contains("not an ancestor"));
    let failure = rig.done(&id, 1).unwrap_err();
    assert_eq!(failure.exit, 2);
    assert!(failure
        .message
        .contains(&content::file_digest(&source).unwrap().unwrap()));
    assert!(failure
        .message
        .contains(&content::file_digest(&target).unwrap().unwrap()));
    fs::copy(source, target).unwrap();
    let log = rig.path(&id, "log.txt");
    fs::remove_file(&log).unwrap();
    fs::create_dir(log).unwrap(); // Log failure happens after the journal's durable Done.
    rig.done(&id, 1).unwrap();
    assert!(rig.windows.snapshot(&id).unwrap().steps[0].done);
    rig.done(&id, 1).unwrap();
    assert_eq!(rig.windows.snapshot(&id).unwrap().record_count, 3);
}

#[test]
fn placement_domain_refuses_sources_targets_containing_paths_and_aliases() {
    let mut rig = Rig::new("window-placement-domain");
    let id = rig.begin(None);
    let candidate = rig.file("unprotected", "candidate");
    let root = rig.windows.root().to_path_buf();
    for forbidden in [
        root.join("installer.journal"),
        root.join("installer.lock"),
        root.join("stack.lock"),
        root.join("stack-stopped"),
        root.clone(),
        root.join("windows"),
        rig.path(&id, "journal"),
    ] {
        for request in [
            file_request(&forbidden, Some(&candidate)),
            file_request(&candidate, Some(&forbidden)),
        ] {
            let failure = rig.record(&id, request).unwrap_err();
            assert_eq!(failure.exit, 2);
            assert!(failure.message.contains("protected path"), "{failure}");
        }
    }
    #[cfg(unix)]
    {
        let alias = rig.base.join("alias");
        std::os::unix::fs::symlink(root.join("windows"), &alias).unwrap();
        assert!(rig
            .record(
                &id,
                file_request(&candidate, Some(&alias.join(&id).join("journal")))
            )
            .unwrap_err()
            .message
            .contains("protected path"));
    }
    assert_eq!(rig.windows.snapshot(&id).unwrap().record_count, 1);
    assert!(!rig.path(&id, "saved").exists());
}

#[test]
fn bundle_overlap_preserves_modes_and_links_and_store_captures_floors() {
    let mut rig = Rig::new("window-bundle-store");
    let id = rig.begin(None);
    let prior = rig.file("App/prg", "prior");
    let after = rig.file("New/prg", "after");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&prior, fs::Permissions::from_mode(0o751)).unwrap();
        std::os::unix::fs::symlink("prg", rig.base.join("App/link")).unwrap();
    }
    let target = prior.parent().unwrap().to_owned();
    let new = after.parent().unwrap().to_owned();
    rig.record(
        &id,
        RecordRequest::Bundle {
            target: target.clone(),
            new: new.clone(),
        },
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::read_link(rig.path(&id, "saved/1/link")).unwrap(),
            Path::new("prg")
        );
        assert_eq!(
            fs::metadata(rig.path(&id, "saved/1/prg"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o751
        );
    }
    fs::remove_dir_all(&target).unwrap();
    fs::rename(&new, &target).unwrap();
    rig.done(&id, 1).unwrap();
    let unrelated = rig.file("Nested/a", "content");
    for candidate in [target.clone(), target.join("prg")] {
        assert!(rig
            .record(&id, file_request(&candidate, Some(&unrelated)))
            .unwrap_err()
            .message
            .contains("overlaps earlier bundle target"));
    }
    rig.record(&id, file_request(&unrelated, Some(&unrelated)))
        .unwrap();
    rig.done(&id, 2).unwrap();
    assert!(rig
        .record(
            &id,
            RecordRequest::Bundle {
                target: unrelated.parent().unwrap().into(),
                new: target
            }
        )
        .unwrap_err()
        .message
        .contains("overlaps earlier file target"));
    let store = rig.file("data/db.sqlite", "live store");
    let snapshot = rig.file("store-snapshot", "snapshotted store");
    let root_floor = rig.file("data/format-floor.json", "root floor");
    let nested_floor = rig.file("data/nested/format-floor.json", "nested floor");
    rig.record(
        &id,
        RecordRequest::Store {
            store,
            snapshot,
            module: "worker".into(),
        },
    )
    .unwrap();
    rig.done(&id, 3).unwrap();
    let journal = rig.windows.snapshot(&id).unwrap();
    let Action::Store {
        snapshot_sha256,
        floors,
        ..
    } = &journal.steps[2].step.action
    else {
        panic!("store step")
    };
    assert_eq!(
        snapshot_sha256,
        &content::file_digest(&rig.path(&id, "saved/3/snapshot"))
            .unwrap()
            .unwrap()
    );
    assert_eq!(floors.len(), 2);
    for (floor, expected) in floors.iter().zip([root_floor, nested_floor]) {
        assert_eq!(
            floor.sha256,
            content::file_digest(&expected).unwrap().unwrap()
        );
        assert_eq!(
            fs::read(rig.path(&id, "saved/3/floors").join(&floor.path)).unwrap(),
            fs::read(expected).unwrap()
        );
    }
}

#[test]
fn same_filesystem_store_record_takes_ownership_of_snapshot() {
    use sha2::{Digest, Sha256};

    let mut rig = Rig::new("window-store-move");
    let id = rig.begin(None);
    let store = rig.file("data/store.db", "live store");
    let snapshot = rig.file("snapshot", "bytes from the snapshot");
    rig.record(
        &id,
        RecordRequest::Store {
            store,
            snapshot: snapshot.clone(),
            module: "worker".into(),
        },
    )
    .unwrap();

    assert!(
        matches!(fs::symlink_metadata(&snapshot), Err(error) if error.kind() == io::ErrorKind::NotFound),
        "the window must take ownership of a same-filesystem snapshot"
    );
    let saved = rig.path(&id, "saved/1/snapshot");
    let expected = format!("{:x}", Sha256::digest(b"bytes from the snapshot"));
    assert_eq!(
        content::file_digest(&saved).unwrap(),
        Some(expected.clone())
    );
    let journal = rig.windows.snapshot(&id).unwrap();
    assert!(
        matches!(&journal.steps[0].step.action, Action::Store { snapshot_sha256, snapshot_transfer: SnapshotTransfer::Moved, .. } if snapshot_sha256 == &expected)
    );
    assert!(fs::read_to_string(rig.path(&id, "log.txt"))
        .unwrap()
        .contains("\"snapshot moved\""));
}

#[test]
fn same_filesystem_store_move_only_reserves_space_for_floors() {
    let mut rig = Rig::new("window-store-move-space");
    let id = rig.begin(None);
    let store = rig.file("data/store.db", "live data");
    let floor = rig.file("data/format-floor.json", "floor");
    let snapshot = rig.file("snapshot", "snapshot data larger than the floor");
    content::SPACE.with(|space| space.set(Some(5)));
    rig.record(
        &id,
        RecordRequest::Store {
            store,
            snapshot: snapshot.clone(),
            module: "worker".into(),
        },
    )
    .unwrap();
    content::SPACE.with(|space| space.set(None));
    assert!(fs::symlink_metadata(snapshot).is_err());
    assert_eq!(fs::read(floor).unwrap(), b"floor");
    assert_eq!(
        fs::read(rig.path(&id, "saved/1/floors/format-floor.json")).unwrap(),
        b"floor"
    );
}

#[test]
fn cross_filesystem_store_copy_checks_space_and_readback_and_keeps_original() {
    let mut rig = Rig::new("window-store-copy");
    let id = rig.begin(None);
    let store = rig.file("data/store.db", "live data");
    let snapshot = rig.file("snapshot", "snapshot data");
    let request = || RecordRequest::Store {
        store: store.clone(),
        snapshot: snapshot.clone(),
        module: "worker".into(),
    };
    durable::CROSS_DEVICE_FROM.with(|source| source.replace(Some(snapshot.clone())));
    content::SPACE.with(|space| space.set(Some(0)));
    let refused = rig.record(&id, request()).unwrap_err();
    assert!(refused.message.contains("not enough free space"));
    assert_eq!(refused.exit, 2);
    content::SPACE.with(|space| space.set(None));
    content::TRUNCATE.with(|flag| flag.set(true));
    assert!(rig
        .record(&id, request())
        .unwrap_err()
        .message
        .contains("does not match source digest"));
    assert!(rig.windows.snapshot(&id).unwrap().steps.is_empty());
    assert_eq!(fs::read(&snapshot).unwrap(), b"snapshot data");
    rig.record(&id, request()).unwrap();
    durable::CROSS_DEVICE_FROM.with(|source| source.take());
    assert_eq!(fs::read(snapshot).unwrap(), b"snapshot data");
    assert_eq!(
        fs::read(rig.path(&id, "saved/1/snapshot")).unwrap(),
        b"snapshot data"
    );
    assert!(matches!(
        rig.windows.snapshot(&id).unwrap().steps[0].step.action,
        Action::Store {
            snapshot_transfer: SnapshotTransfer::Copied,
            ..
        }
    ));
    assert!(fs::read_to_string(rig.path(&id, "log.txt"))
        .unwrap()
        .contains("\"snapshot copied\""));
}

#[test]
fn record_cannot_take_a_live_store_sidecar_or_floor_as_its_snapshot() {
    let mut rig = Rig::new("window-store-live-names");
    let id = rig.begin(None);
    let store = rig.file("data/store.db", "live database");
    let wal = rig.file("data/store.db-wal", "live WAL");
    let floor = rig.file("data/format-floor.json", "live floor");
    for path in [&store, &wal, &floor] {
        let refused = rig
            .record(
                &id,
                RecordRequest::Store {
                    store: store.clone(),
                    snapshot: path.clone(),
                    module: "worker".into(),
                },
            )
            .unwrap_err();
        assert!(refused.message.contains("provide a separate snapshot"));
        assert_eq!(refused.exit, 2);
        assert!(path.is_file());
    }
    assert!(!rig.path(&id, "saved").exists());
}

#[test]
fn directory_and_file_syncs_precede_begin_and_store_publication_records() {
    let mut rig = Rig::new("window-sync-order");
    let id = id::candidate(NOW, 1);
    let temporary = rig.path(&format!(".tmp-{id}"), "");
    durable::EVENTS.with(|events| events.borrow_mut().clear());
    rig.begin(None);
    let windows = rig.windows.root().join("windows");
    let event = |kind, path: PathBuf| (kind, path);
    let events = durable::EVENTS.with(|events| events.take());
    assert_eq!(
        events,
        [
            event("parent", windows),
            event("parent", temporary.components().collect()),
            event("file", temporary.join("journal")),
            event("parent", temporary.join("journal")),
            event("parent", rig.path(&id, "").components().collect())
        ]
    );
    let store = rig.file("db/live", "live");
    let snapshot = rig.file("snapshot", "snapshot");
    rig.file("db/nested/format-floor.json", "floor");
    rig.record(
        &id,
        RecordRequest::Store {
            store,
            snapshot: snapshot.clone(),
            module: "worker".into(),
        },
    )
    .unwrap();
    let events = durable::EVENTS.with(|events| events.take());
    let first = |kind, relative| {
        events
            .iter()
            .position(|entry| *entry == event(kind, rig.path(&id, relative)))
            .unwrap()
    };
    assert!(first("parent", "saved") < first("parent", "saved/.tmp-1"));
    assert!(first("parent", "saved/.tmp-1") < first("parent", "saved/.tmp-1/floors"));
    assert!(
        first("parent", "saved/.tmp-1/floors/nested")
            < first("file", "saved/.tmp-1/floors/nested/format-floor.json")
    );
    let source_event = |kind| {
        events
            .iter()
            .position(|entry| *entry == event(kind, snapshot.clone()))
            .unwrap()
    };
    assert!(source_event("file") < first("parent", "saved/.tmp-1/snapshot"));
    assert!(first("parent", "saved/.tmp-1/snapshot") < source_event("parent"));
    assert!(source_event("parent") < first("parent", "saved/1"));
    assert!(first("parent", "saved/1") < first("file", "journal"));
}

#[test]
fn stamped_and_unconfirmed_begin_refuse_without_writes() {
    let mut rig = Rig::new("window-stamped");
    let id = id::candidate(NOW, 1);
    rig.host.stamped = Some(true);
    assert_eq!(
        rig.windows
            .begin_with(None, &mut rig.host, NOW, || 1, || Ok(()))
            .unwrap_err()
            .message,
        PROTECTED
    );
    assert_eq!(
        rig.windows.current_with(&mut rig.host).unwrap_err().message,
        PROTECTED
    );
    assert_eq!(
        rig.close(None, Closing::Keep).unwrap_err().message,
        PROTECTED
    );
    assert_eq!(
        rig.windows.list_with(&mut rig.host).unwrap_err().message,
        PROTECTED
    );
    assert!(!rig.windows.root().join("windows").exists());
    rig.host.stamped = None;
    assert_eq!(
        rig.windows
            .begin_with(None, &mut rig.host, NOW, || 1, || Ok(()))
            .unwrap_err()
            .message,
        UNCONFIRMED
    );
    assert!(!rig.windows.root().join("windows").exists());
    rig.host.stamped = Some(false);
    assert_eq!(rig.begin(None), id);
    rig.host.stamped = Some(true);
    let target = rig.file("file", "old bytes");
    let snapshot = rig.file("snapshot", "db content");
    let tagged = RecordRequest::File {
        target: target.clone(),
        new: None,
        module: Some("worker".into()),
        role: Some(Role::Binary),
    };
    let store = RecordRequest::Store {
        store: target,
        snapshot,
        module: "worker".into(),
    };
    assert_eq!(rig.record(&id, tagged).unwrap_err().message, PROTECTED);
    assert_eq!(rig.record(&id, store).unwrap_err().message, PROTECTED);
    assert_eq!(
        rig.close(Some(&id), Closing::Abandon).unwrap_err().message,
        PROTECTED
    );
    assert!(!rig.path(&id, "saved").exists());
    rig.host.stamped = None;
    rig.close(Some(&id), Closing::Abandon).unwrap();
}

#[tokio::test]
async fn reachable_daemon_without_local_endpoint_is_not_offline() {
    use std::{net::Ipv4Addr, time::Duration};
    use subc_transport::{
        authenticate_server, generate_daemon_id, generate_key, read_frame, write_atomic,
        ConnectionInfo, Endpoint, SCHEMA_VERSION,
    };
    use tokio::{net::TcpListener, time::timeout};

    let root = ScratchDir::new("window-tcp-only");
    let connection = root.join("connection.json");
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let info = ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: Some(subc_protocol::PROTOCOL_VERSION),
        endpoints: vec![Endpoint {
            host: Ipv4Addr::LOCALHOST.to_string(),
            port: listener.local_addr().unwrap().port(),
        }],
        key: generate_key().unwrap(),
        daemon_id: generate_daemon_id().unwrap(),
        pid: std::process::id(),
        daemon_ver: "test-daemon".into(),
    };
    write_atomic(&connection, &info).unwrap();
    let windows = Windows::at(root.path(), Some(&connection)).unwrap();
    let client =
        tokio::task::spawn_blocking(move || windows.current_with(&mut SystemHost(&windows)));
    let serve_probe = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        authenticate_server(
            &mut stream,
            &info.key,
            &info.daemon_id,
            &info.daemon_ver,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert!(
            read_frame(&mut stream).await.unwrap().is_none(),
            "the TCP probe must not send a control request"
        );
    };
    let (result, ()) = timeout(Duration::from_secs(5), async {
        tokio::join!(client, serve_probe)
    })
    .await
    .expect("the authenticated TCP probe completes");
    let error = result.unwrap().unwrap_err();
    assert_eq!(error.exit, 2);
    assert!(
        error
            .message
            .contains("has no endpoint for the requested transport"),
        "{error}"
    );
}

#[test]
fn activation_requires_new_incarnation_and_recorded_daemon_content() {
    let mut rig = Rig::new("window-daemon-activation");
    let id = rig.begin(None);
    let daemon = rig.file("ck-subc", "A");
    let candidate = rig.file("new", "B");
    rig.host.daemon_path = daemon.clone();
    rig.host.manager = Err("manager failed".into());
    assert!(rig
        .record(&id, activation("daemon"))
        .unwrap_err()
        .message
        .contains("manager failed"));
    rig.host.manager = Ok(true);
    assert!(rig
        .record(&id, activation("daemon"))
        .unwrap_err()
        .message
        .contains("identity is unavailable"));
    rig.host.daemon = Some(identity(30, 123));
    assert!(rig
        .record(&id, activation("daemon"))
        .unwrap_err()
        .message
        .contains("not recorded"));
    rig.host.image_proof = true;
    assert!(rig
        .record(&id, activation("daemon"))
        .unwrap_err()
        .message
        .contains("not recorded"));
    rig.record(&id, file_request(&daemon, Some(&candidate)))
        .unwrap();
    fs::write(&daemon, b"B").unwrap();
    rig.done(&id, 1).unwrap();
    fs::write(&daemon, b"C").unwrap();
    assert!(rig
        .record(&id, activation("daemon"))
        .unwrap_err()
        .message
        .contains(daemon.to_str().unwrap()));
    fs::write(&daemon, b"B").unwrap();
    rig.record(&id, activation("daemon")).unwrap();
    assert!(rig
        .done(&id, 2)
        .unwrap_err()
        .message
        .contains("still the recorded incarnation"));
    rig.host.daemon = None;
    assert!(rig
        .done(&id, 2)
        .unwrap_err()
        .message
        .contains("did not report"));
    rig.host.daemon = Some(identity(30, 124));
    assert!(rig
        .done(&id, 2)
        .unwrap_err()
        .message
        .contains("running image does not match"));
    rig.host.image_matches = true;
    rig.done(&id, 2).unwrap();
    rig.host.manager = Ok(false);
    rig.host.daemon = None;
    rig.record(&id, activation("daemon")).unwrap();
    rig.host.manager = Ok(true);
    rig.host.daemon = Some(identity(31, 125));
    rig.done(&id, 3).unwrap();
}

#[test]
fn module_activation_requires_list_generation_live_process_and_image_verdict() {
    let mut rig = Rig::new("window-module-activation");
    let id = rig.begin(None);
    assert!(rig
        .record(&id, activation("worker"))
        .unwrap_err()
        .message
        .contains("no module worker"));
    rig.host.module_state = Some((10, true, false));
    rig.record(&id, activation("worker")).unwrap();
    assert!(rig
        .done(&id, 1)
        .unwrap_err()
        .message
        .contains("still on spawn generation"));
    rig.host.module_state = Some((11, false, false));
    assert!(rig
        .done(&id, 1)
        .unwrap_err()
        .message
        .contains("no proven live process"));
    rig.host.module_state = Some((11, true, true));
    assert!(rig
        .done(&id, 1)
        .unwrap_err()
        .message
        .contains("running-image disagreement"));
    rig.host.module_state = Some((11, true, false));
    rig.done(&id, 1).unwrap();
}

#[test]
fn unreadable_or_dead_owners_drive_interruption_and_abandon_preserves_artifacts() {
    let mut rig = Rig::new("window-owner-lifecycle");
    let id = rig.begin(None);
    let target = rig.file("target", "old bytes");
    rig.record(&id, file_request(&target, None)).unwrap();
    assert!(rig.list().human().contains("step 1 of 1 busy"));
    assert!(rig
        .windows
        .begin_with(None, &mut rig.host, NOW, || 2, || Ok(()))
        .unwrap_err()
        .message
        .contains("open"));
    rig.host.liveness = Liveness::Unreadable;
    assert!(rig
        .close(Some(&id), Closing::Abandon)
        .unwrap_err()
        .message
        .contains("identity unreadable"));
    rig.host.liveness = Liveness::Gone;
    assert!(rig
        .list()
        .human()
        .contains("interrupted: stopped at step 1 of 1"));
    assert!(rig
        .windows
        .current_with(&mut rig.host)
        .unwrap_err()
        .message
        .contains("interrupted"));
    assert!(rig
        .close(Some(&id), Closing::Keep)
        .unwrap_err()
        .message
        .contains("ck window revert"));
    let mut locked = rig.windows.lock(&id, 2).unwrap();
    locked
        .append(Record::RevertStart {
            owner: identity(41, 10),
        })
        .unwrap();
    drop(locked);
    rig.host.liveness = Liveness::Unreadable;
    assert!(rig
        .close(Some(&id), Closing::Abandon)
        .unwrap_err()
        .message
        .contains("identity unreadable"));
    rig.host.liveness = Liveness::Gone;
    let rows = rig.list();
    assert_eq!(rows.windows[0].state, State::RevertInterrupted);
    assert_eq!(rows.windows[0].revert_status, Some("stopped_part_way"));
    let result = rig.close(Some(&id), Closing::Abandon).unwrap();
    assert_eq!(result.outstanding[0].index, 1);
    assert!(rig.path(&id, "saved/1").is_file());
    assert!(rig.path(&id, "log.txt").is_file());
    assert!(rig
        .close(Some(&id), Closing::Keep)
        .unwrap_err()
        .message
        .contains("abandoned"));
    assert_eq!(
        rig.windows
            .begin_with(None, &mut rig.host, NOW, || 2, || Ok(()))
            .unwrap(),
        id::candidate(NOW, 2)
    );
}

#[cfg(unix)]
#[test]
fn earlier_boot_is_gone_even_when_pid_and_start_time_match() {
    let identity = identity(10, 123);
    let other_boot = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
    assert_eq!(
        identity.compare(Ok(&other_boot), || ProcessReading::Alive(123)),
        Liveness::Gone
    );
    assert_eq!(
        identity.compare(Err(()), || ProcessReading::Alive(123)),
        Liveness::Unreadable
    );
}

#[test]
fn kept_cleanup_resumes_and_prompt_plan_is_pinned() {
    let mut rig = Rig::new("window-keep-interrupted");
    let id = rig.begin(Some("roll \"back\"\u{202e}"));
    let old = rig.file("target", "old");
    let new = rig.file("new", "new");
    let request = || file_request(&old, Some(&new));
    let mut mock = Fake::default();
    let error = rig.windows.close(Some(&id), Closing::Keep, &mut rig.host, |prompt| {
        assert_eq!(prompt, format!("ck window keep: keep window {id} (0 steps) and delete its saved copies of the previous builds? Note: \"roll \\\"back\\\"\\u202e\""));
        rig.windows.record_with(&id, 10, request(), &mut mock).unwrap();
        fs::copy(&new, &old).unwrap();
        rig.windows.done_with(&id, 10, 1, &mut mock).unwrap();
        Ok(())
    }).unwrap_err();
    assert!(error.message.contains("changed while confirming"));
    assert_eq!(rig.list().windows[0].state, State::Open);
    let mut locked = rig.windows.lock(&id, 3).unwrap();
    locked.append(Record::Kept).unwrap();
    drop(locked);
    assert!(rig.list().human().contains("kept, cleanup pending"));
    assert!(rig.windows.revert_candidate(None).is_err());
    let close = rig
        .windows
        .close(None, Closing::Keep, &mut rig.host, |prompt| {
            assert!(prompt.contains("(1 step)"));
            Ok(())
        })
        .unwrap();
    assert_eq!(close.id, id);
    assert!(!rig.path(&id, "saved").exists());
    assert!(rig
        .close(Some(&id), Closing::Keep)
        .unwrap_err()
        .message
        .contains("cleanup is complete"));
    assert_eq!(rig.windows.snapshot(&id).unwrap().record_count, 4);
}

#[test]
fn record_discards_torn_tail_and_reuses_index() {
    use std::io::Write as _;
    let mut rig = Rig::new("window-torn-tail");
    let id = rig.begin(None);
    let path = rig.path(&id, "journal");
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    write!(file, "{{\"record\":\"step\",\"step\":{{").unwrap();
    file.sync_all().unwrap();
    drop(file);
    assert_eq!(rig.windows.snapshot(&id).unwrap().record_count, 1);
    let target = rig.file("prior", "remove me");
    assert_eq!(rig.record(&id, file_request(&target, None)).unwrap(), 1);
    fs::remove_file(target).unwrap();
    rig.done(&id, 1).unwrap();
    assert_eq!(fs::read_to_string(path).unwrap().lines().count(), 3);
    assert_eq!(rig.windows.snapshot(&id).unwrap().record_count, 3);
}

#[test]
fn journal_sync_failure_does_not_leave_a_done_record() {
    let mut rig = Rig::new("window-failed-sync");
    let id = rig.begin(None);
    let target = rig.file("target", "old bytes");
    rig.record(&id, file_request(&target, None)).unwrap();
    fs::remove_file(target).unwrap();
    let journal = rig.path(&id, "journal");
    let before = fs::read(&journal).unwrap();
    durable::FAIL_SYNC.with(|fault| fault.replace(Some(journal.clone())));
    let failure = rig.done(&id, 1).unwrap_err();
    assert_eq!(failure.exit, 1);
    assert!(failure.message.contains("injected file sync failure"));
    assert_eq!(fs::read(journal).unwrap(), before);
    assert!(!rig.windows.snapshot(&id).unwrap().steps[0].done);
    rig.done(&id, 1).unwrap();
}

#[cfg(unix)]
#[test]
fn named_window_with_nonprivate_journal_is_unreadable() {
    use std::os::unix::fs::PermissionsExt;

    let mut rig = Rig::new("window-owner-only");
    let id = rig.begin(None);
    let path = rig.path(&id, "journal");
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(rig.list().windows[0].state, State::Unreadable);
    assert!(rig
        .windows
        .snapshot(&id)
        .unwrap_err()
        .message
        .contains("owner-only"));
}

#[cfg(target_os = "linux")]
#[test]
fn running_image_check_does_not_use_the_replacement_at_a_path() {
    let rig = Rig::new("window-real-image");
    let host = SystemHost(&rig.windows);
    let instance = Owner::current().unwrap();
    let image = std::env::current_exe().unwrap();
    let another_file = rig.file("replacement", "does not match the image");
    assert!(host.image_matches(&instance, &image));
    assert!(!host.image_matches(&instance, &another_file));
    assert!(host.owner(std::process::id()).is_err());
}
