use super::*;
use serde_json::{json, Value};

struct RegistryScratch {
    _scratch: cortexkit_test_support::ScratchDir,
    root: PathBuf,
}

impl RegistryScratch {
    fn new(name: &str) -> Self {
        let scratch = cortexkit_test_support::ScratchDir::new(name);
        // macOS can put scratch directories below the /var symlink. Registry
        // fixtures should start from canonical roots before introducing links.
        #[cfg(unix)]
        let root = scratch.canonicalize().unwrap();
        #[cfg(windows)]
        let root = scratch.to_path_buf();
        Self {
            _scratch: scratch,
            root,
        }
    }
}

impl std::ops::Deref for RegistryScratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.root
    }
}

fn file_id(path: &Path) -> String {
    let file = subc_os::open_directory_nofollow_all(path).unwrap();
    #[cfg(unix)]
    {
        let id = subc_os::retained_file_identity(&file).unwrap();
        format!("unix:{}:{}", id.device, id.inode)
    }
    #[cfg(windows)]
    {
        let id = subc_os::retained_file_index(&file).unwrap();
        format!("windows:{}:{}", id.volume_serial, id.file_index)
    }
}

fn root(path: &Path) -> Value {
    json!({"root":path, "remotes":[], "incarnation":{"kind":"token","value":"checkout-one"},
        "registrationEpoch":null, "identity":"bound", "approval":{"state":"approved","journalSeq":1}, "fileId":file_id(path)})
}

fn project(id: &str, roots: Vec<Value>) -> Value {
    json!({"projectId":id, "name":id, "roots":roots.iter().map(|root| root["root"].clone()).collect::<Vec<_>>(),
        "implicit":false, "refKind":"local", "workspaceId":"workspace", "rootRecords":roots})
}

fn enumeration(projects: Vec<Value>, workspace: &Path) -> EnumerateReply {
    serde_json::from_value(json!({"projects":projects,"workspaces":[{"workspaceId":"workspace","name":"Workspace","root":workspace,"fileId":file_id(workspace),"approval":{"state":"approved","journalSeq":1}}],
        "generation":7,"incarnation":"registry-process"})).unwrap()
}

async fn snapshot(reply: &EnumerateReply) -> Result<RegistryRevision> {
    let pool = RootIoPool::new().unwrap();
    RegistryRevision::from_enumerate(&pool, reply, "module", &BTreeSet::from(["project".into()]))
        .await
}

#[test]
fn registry_file_id_parses_both_forms_and_refuses_malformed_text() {
    assert_eq!(
        RegistryFileId::parse("unix:18446744073709551615:2").unwrap(),
        RegistryFileId::Unix {
            device: u64::MAX,
            inode: 2
        }
    );
    assert_eq!(
        RegistryFileId::parse("windows:4294967295:18446744073709551615").unwrap(),
        RegistryFileId::Windows {
            volume_serial: u32::MAX,
            file_index: u64::MAX
        }
    );
    for bad in [
        "unix:1",
        "unix:1:2:3",
        "unix:+1:2",
        "unix:-1:2",
        "unix::2",
        "unix: 1:2",
        "unix:0x10:2",
        "unix:18446744073709551616:2",
        "windows:4294967296:2",
        "other:1:2",
        "windows:1:2\n",
    ] {
        assert!(RegistryFileId::parse(bad).is_err(), "accepted {bad:?}");
    }
}

#[tokio::test]
async fn registry_snapshot_workspace_then_project_from_fake_enumerate() {
    let dir = RegistryScratch::new("registry-snapshot");
    let workspace = dir.join("workspace");
    let local = dir.join("project");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&local).unwrap();
    let mut reply = enumeration(vec![project("project", vec![])], &workspace);
    let saved = snapshot(&reply).await.unwrap();
    assert!(saved.projects[0].project_roots.is_empty());
    assert_eq!(
        saved.projects[0].workspace_root.as_ref().unwrap().path,
        workspace
    );
    reply.projects[0] = serde_json::from_value(project("project", vec![root(&local)])).unwrap();
    let saved = snapshot(&reply).await.unwrap();
    assert_eq!(saved.projects[0].project_roots[0].path, local);
    assert_eq!(
        saved.projects[0].workspace_root.as_ref().unwrap().path,
        workspace
    );
}

#[test]
fn registry_root_selection_zero_ineligible_one_and_multiple_candidates() {
    let dir = RegistryScratch::new("registry-candidates");
    let first = dir.join("a");
    let second = dir.join("b");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();
    let mut empty: Project = serde_json::from_value(project("project", vec![])).unwrap();
    empty.ref_kind = cortexkit_project_identity::RefKind::Remote;
    assert!(project_layer_root(&empty, "module", &BTreeSet::new())
        .unwrap()
        .is_none());
    for (identity, approval) in [
        ("unbound", "approved"),
        ("replaced", "approved"),
        ("bound", "unapproved"),
        ("unknown", "approved"),
    ] {
        let mut record = root(&first);
        record["identity"] = json!(identity);
        record["approval"]["state"] = json!(approval);
        let entry: Project = serde_json::from_value(project("project", vec![record])).unwrap();
        assert!(
            project_layer_root(
                &entry,
                "module",
                &BTreeSet::from([layer_path(&first, "module")])
            )
            .unwrap()
            .is_none(),
            "{identity}/{approval}"
        );
    }
    let entry: Project =
        serde_json::from_value(project("project", vec![root(&first), root(&second)])).unwrap();
    assert!(project_layer_root(&entry, "module", &BTreeSet::new())
        .unwrap()
        .is_none());
    assert_eq!(
        project_layer_root(
            &entry,
            "module",
            &BTreeSet::from([layer_path(&first, "module")])
        )
        .unwrap()
        .unwrap()
        .root,
        first.to_string_lossy()
    );
    let error = project_layer_root(
        &entry,
        "module",
        &BTreeSet::from([layer_path(&first, "module"), layer_path(&second, "module")]),
    )
    .unwrap_err();
    assert_eq!(error.code, "layer_root_ambiguous");
    assert!(error.detail.contains(first.to_str().unwrap()));
    assert!(error.detail.contains(second.to_str().unwrap()));
}

#[tokio::test]
async fn registry_identity_absent_malformed_mismatched_and_incarnation_only() {
    let dir = RegistryScratch::new("registry-identities");
    let mut reply = enumeration(vec![project("project", vec![root(&dir)])], &dir);
    reply.workspaces[0].file_id = None;
    let revision = snapshot(&reply).await.unwrap();
    let error = revision.project_layers("project").err().unwrap();
    assert_eq!(error.code, "config_unavailable");
    assert!(error.detail.contains("project"));
    reply.workspaces[0].file_id = Some(file_id(&dir));
    for (bad, expected) in [
        ("broken", "invalid registry fileId"),
        ("unix:0:0", "root fileId changed"),
    ] {
        reply.projects[0].root_records.as_mut().unwrap()[0].file_id = Some(bad.into());
        let revision = snapshot(&reply).await.unwrap();
        let error = revision.project_layers("project").err().unwrap();
        assert_eq!(error.code, "config_unavailable");
        assert!(error.detail.contains(expected));
    }
    let record = &mut reply.projects[0].root_records.as_mut().unwrap()[0];
    record.file_id = None;
    assert!(!snapshot(&reply).await.unwrap().projects[0]
        .project_roots
        .is_empty());
    reply.projects[0].root_records.as_mut().unwrap()[0].incarnation = None;
    assert!(snapshot(&reply).await.unwrap().projects[0]
        .project_roots
        .is_empty());
}

#[tokio::test]
async fn registry_commit_recheck_ignores_unrelated_edits_but_refuses_affected_changes() {
    let dir = RegistryScratch::new("registry-recheck");
    let reply = enumeration(
        vec![
            project("project", vec![root(&dir)]),
            project("unrelated", vec![]),
        ],
        &dir,
    );
    let saved = snapshot(&reply).await.unwrap();
    let scope = RegistryScope::new(
        BTreeSet::from(["project".into()]),
        BTreeSet::new(),
        BTreeSet::new(),
    );
    let mut unrelated = reply.clone();
    unrelated.generation += 1;
    unrelated.incarnation = "another-registry-process".into();
    unrelated.projects[1].workspace_id = None;
    let current = snapshot(&unrelated).await.unwrap();
    saved.recheck(&current, &scope).unwrap();
    assert_eq!(saved.digest().unwrap(), current.digest().unwrap());
    for change in [
        "file_id",
        "workspace",
        "root",
        "incarnation",
        "approval",
        "project_id",
    ] {
        let mut current = saved.clone();
        let entry = &mut current.projects[0];
        match change {
            "file_id" => {
                entry.project_roots.first_mut().unwrap().file_id = Some(RegistryFileId::Unix {
                    device: 0,
                    inode: 0,
                })
            }
            "workspace" => entry.workspace_id = Some("moved".into()),
            "root" => entry.project_roots.first_mut().unwrap().path = dir.join("moved"),
            "incarnation" => entry.project_roots.first_mut().unwrap().incarnation = None,
            "approval" => {
                entry
                    .project_roots
                    .first_mut()
                    .unwrap()
                    .approval
                    .as_mut()
                    .unwrap()
                    .state = ApprovalState::Unapproved
            }
            "project_id" => entry.project_id = "replaced-project".into(),
            _ => unreachable!(),
        }
        assert_eq!(
            saved.recheck(&current, &scope).unwrap_err().code,
            "config_changed_since_review",
            "{change}"
        );
    }
}

#[tokio::test]
async fn registry_recheck_derives_readers_from_a_fresh_enumeration() {
    let dir = RegistryScratch::new("registry-reread");
    let private = dir.join("private");
    std::fs::create_dir(&private).unwrap();
    let reply = enumeration(
        vec![
            project("project", vec![root(&private)]),
            project("unrelated", vec![]),
        ],
        &dir,
    );
    let mut before = reply.clone();
    before.projects[1].workspace_id = None;
    let reader = FakeRegistry::new(before.clone());
    for (path, workspace) in [
        (layer_path(&private, "module"), None),
        (layer_path(&dir, "module"), Some("workspace")),
    ] {
        let scope = RegistryScope::new(
            BTreeSet::new(),
            BTreeSet::from([path]),
            workspace.map(str::to_owned).into_iter().collect(),
        );
        let saved = reader.snapshot_scoped("module", &scope).await.unwrap();
        let mut unrelated = before.clone();
        unrelated.projects[1].project_id = "unrelated-renamed".into();
        reader.set(Some(unrelated));
        reader.recheck(&saved, &scope).await.unwrap();

        let mut after = before.clone();
        if workspace.is_some() {
            after.projects[1].workspace_id = Some("workspace".into());
        } else {
            after.projects[1].root_records = after.projects[0].root_records.clone();
        }
        reader.set(Some(after));
        assert_eq!(
            reader.recheck(&saved, &scope).await.unwrap_err().code,
            "config_changed_since_review"
        );

        reader.set(None);
        assert_eq!(
            reader.recheck(&saved, &scope).await.unwrap_err().code,
            "project_registry_unavailable"
        );
        reader.set(Some(before.clone()));
    }
}

struct FakeRegistry(std::sync::Mutex<Option<EnumerateReply>>, RootIoPool);

impl FakeRegistry {
    fn new(reply: EnumerateReply) -> Self {
        Self(
            std::sync::Mutex::new(Some(reply)),
            RootIoPool::new().unwrap(),
        )
    }

    fn set(&self, reply: Option<EnumerateReply>) {
        *self.0.lock().unwrap() = reply;
    }
}

impl ProjectRegistry for FakeRegistry {
    fn snapshot_scoped<'a>(
        &'a self,
        module: &'a str,
        scope: &'a RegistryScope,
    ) -> RegistryFuture<'a> {
        Box::pin(async move {
            let reply = self
                .0
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| unavailable("test enumeration is offline"))?;
            RegistryRevision::from_scope(&self.1, &reply, module, scope).await
        })
    }
}

#[cfg(windows)]
#[test]
fn registry_windows_real_directory_file_index_matches_and_mismatch_refuses() {
    let dir = RegistryScratch::new("registry-windows-index");
    let file = subc_os::open_directory_nofollow_all(&dir).unwrap();
    let identity = RegistryFileId::parse(&file_id(&dir)).unwrap();
    assert_eq!(identity, RegistryFileId::read(&file).unwrap());
    let RegistryFileId::Windows {
        volume_serial,
        file_index,
    } = identity
    else {
        panic!("not Windows")
    };
    assert_ne!(
        RegistryFileId::Windows {
            volume_serial,
            file_index: file_index ^ 1
        },
        RegistryFileId::read(&file).unwrap()
    );
}

#[tokio::test]
async fn registry_snapshot_validates_25_roots_with_bounded_submissions() {
    let dir = RegistryScratch::new("registry-many-roots");
    let mut projects = Vec::new();
    let mut affected = BTreeSet::new();
    for index in 0..25 {
        let id = format!("project-{index:02}");
        let path = dir.join(&id);
        std::fs::create_dir(&path).unwrap();
        projects.push(project(&id, vec![root(&path)]));
        affected.insert(id);
    }
    let reply = enumeration(projects, &dir);
    let pool = RootIoPool::new().unwrap();
    let revision = RegistryRevision::from_enumerate(&pool, &reply, "module", &affected)
        .await
        .unwrap();
    assert_eq!(revision.projects.len(), 25);
    assert!(pool.health().is_none());
}

#[tokio::test]
async fn registry_root_outcomes_keep_healthy_and_refused_roots_distinct() {
    use std::{sync::mpsc, time::Duration};

    let dir = RegistryScratch::new("registry-root-outcomes");
    let [blocked, gone, mismatched, healthy] =
        ["blocked", "gone", "mismatch", "healthy"].map(|name| {
            let path = dir.join(name);
            std::fs::create_dir(&path).unwrap();
            path
        });
    let mut mismatch = root(&mismatched);
    mismatch["fileId"] = json!(file_id(&healthy));
    let reply = enumeration(
        vec![project(
            "project",
            vec![root(&blocked), root(&gone), mismatch, root(&healthy)],
        )],
        &dir,
    );
    std::fs::remove_dir(&gone).unwrap();
    // Every unblocked root must finish its open inside this deadline, so it is
    // generous: on a loaded Windows CI runner a 0.3 s deadline turned the gone
    // root's open into a pool timeout instead of OpenRefused. Only the blocked
    // root is meant to reach it.
    let pool = RootIoPool::with_timeout(Duration::from_secs(5)).unwrap();
    let (release, wait) = mpsc::channel();
    let (entered, signal) = tokio::sync::oneshot::channel();
    let job = pool
        .submit(blocked.clone(), move || {
            entered.send(()).unwrap();
            let _ = wait.recv();
        })
        .unwrap();
    signal.await.unwrap();
    assert_eq!(job.wait().await, Err(RootIoError::DeadlineElapsed));

    let affected = BTreeSet::from(["project".into()]);
    let revision = RegistryRevision::from_enumerate(&pool, &reply, "module", &affected)
        .await
        .unwrap();
    let outcomes = &revision.opened;
    assert!(
        matches!(
            outcomes[&blocked].as_ref(),
            Err(RootCheckFailure::Pool(RootIoError::RootBusy))
        ),
        "blocked root: {:?}",
        outcomes[&blocked].as_ref().as_ref().err()
    );
    assert!(
        matches!(
            outcomes[&gone].as_ref(),
            Err(RootCheckFailure::OpenRefused(_))
        ),
        "gone root: {:?}",
        outcomes[&gone].as_ref().as_ref().err()
    );
    let mismatch = revision
        .layer_location("project", &mismatched)
        .err()
        .unwrap();
    assert_eq!(mismatch.code, "config_unavailable");
    assert!(mismatch.detail.contains("fileId changed"));
    assert!(outcomes[&mismatched]
        .as_ref()
        .as_ref()
        .unwrap()
        .layer
        .is_none());
    assert!(revision.layer_location("project", &healthy).is_ok());
    assert!(revision.layer_location("project", &dir).is_ok());
    let project = revision.project_layers("project").err().unwrap();
    assert_eq!(project.code, "config_unavailable");
    assert_eq!(
        project.detail,
        "config_unavailable: registry-root I/O already outstanding for this root"
    );
    release.send(()).unwrap();
}

#[tokio::test]
async fn registry_snapshots_shared_path_once_under_each_readers_role() {
    let scratch = RegistryScratch::new("registry-shared-layer");
    let shared = scratch.join("shared");
    let separate = scratch.join("separate");
    std::fs::create_dir(&shared).unwrap();
    std::fs::create_dir(&separate).unwrap();
    let reply = enumeration(
        vec![
            project("a", vec![root(&shared)]),
            project("b", vec![root(&separate)]),
        ],
        &shared,
    );
    let reader = FakeRegistry::new(reply.clone());
    let scope = RegistryScope::all();
    let path = layer_path(&shared, "module");
    let before = reader.snapshot_scoped("module", &scope).await.unwrap();
    assert!(before.project_layers("a").unwrap().project.is_none());
    assert!(before.project_layers("b").unwrap().workspace.is_none());
    let readers = BTreeSet::from(["a".to_owned(), "b".to_owned()]);
    assert_eq!(before.readers(&path), readers);
    std::fs::create_dir(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{} // same physical bytes").unwrap();
    let after = reader.snapshot_scoped("module", &scope).await.unwrap();
    let a = after.project_layers("a").unwrap();
    let b = after.project_layers("b").unwrap();
    assert!(a.workspace.is_none());
    assert!(b.project.is_none());
    let (a_root, a_input) = a.project.unwrap();
    let (b_root, b_input) = b.workspace.unwrap();
    assert_eq!(a_root, &shared);
    assert_eq!(b_root, &shared);
    assert!(std::ptr::eq(a_input, b_input));
    assert_eq!(a_input.bytes, b"{} // same physical bytes");
    assert!(!serde_json::to_string(&after)
        .unwrap()
        .contains("same physical bytes"));

    let mut proposed = reply;
    proposed.projects[1].workspace_id = None;
    let new_reader: Project = serde_json::from_value(project("c", vec![root(&shared)])).unwrap();
    proposed.projects.push(new_reader);
    reader.set(Some(proposed));
    let proposed = reader.snapshot_scoped("module", &scope).await.unwrap();
    assert_eq!(
        before.readers_before_or_proposed(&proposed, &path),
        BTreeSet::from(["a".into(), "b".into(), "c".into()])
    );
}

#[tokio::test]
async fn registry_ambiguous_layer_stays_with_its_project() {
    let scratch = RegistryScratch::new("registry-ambiguous-layer");
    let a = scratch.join("a");
    let b = scratch.join("b");
    for root in [&a, &b] {
        std::fs::create_dir_all(root.join(".cortexkit")).unwrap();
        std::fs::write(layer_path(root, "module"), b"{}").unwrap();
    }
    let reader = FakeRegistry::new(enumeration(
        vec![
            project("ambiguous", vec![root(&a), root(&b)]),
            project("other", vec![root(&a)]),
        ],
        &scratch,
    ));
    let revision = reader
        .snapshot_scoped("module", &RegistryScope::all())
        .await
        .unwrap();
    assert_eq!(
        revision.project_layers("ambiguous").err().unwrap().code,
        "layer_root_ambiguous"
    );
    assert!(revision.project_layers("other").unwrap().project.is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn registry_layer_path_safety_refusals_stay_per_project() {
    use std::{fs, os::unix::fs::symlink};
    let scratch = RegistryScratch::new("registry-layer-path-safety");
    let outside = scratch.join("outside");
    fs::create_dir(&outside).unwrap();
    let outside_file = outside.join("module.jsonc");
    fs::write(&outside_file, b"{}").unwrap();
    let mut projects = vec![project("healthy", vec![])];
    let cases = [
        ("linked-root", "layer_symlink"),
        ("linked-component", "layer_symlink"),
        ("linked-leaf", "layer_symlink"),
        ("directory", "layer_not_regular"),
        ("hardlink", "layer_hardlinked"),
        ("relative", "layer_root_invalid"),
        ("noncanonical", "layer_root_invalid"),
        ("oversized", "layer_too_large"),
        ("at-limit", ""),
    ];
    for (name, _) in cases {
        let dir = scratch.join(name);
        fs::create_dir(&dir).unwrap();
        let leaf = layer_path(&dir, "module");
        let mut record = root(&dir);
        match name {
            "linked-root" => {
                fs::remove_dir(&dir).unwrap();
                symlink(&outside, &dir).unwrap();
            }
            "linked-component" => symlink(&outside, leaf.parent().unwrap()).unwrap(),
            "relative" => record["root"] = json!("relative"),
            "noncanonical" => record["root"] = json!(dir.join("../noncanonical")),
            _ => {
                fs::create_dir(leaf.parent().unwrap()).unwrap();
                match name {
                    "linked-leaf" => symlink(&outside_file, leaf).unwrap(),
                    "directory" => fs::create_dir(leaf).unwrap(),
                    "hardlink" => fs::hard_link(&outside_file, leaf).unwrap(),
                    "oversized" | "at-limit" => {
                        let size = crate::config_gate::capture::CONFIG_FILE_LIMIT as usize
                            + usize::from(name == "oversized");
                        let mut contents = vec![b' '; size];
                        contents[..2].copy_from_slice(b"{}");
                        fs::write(leaf, contents).unwrap();
                    }
                    _ => unreachable!(),
                }
            }
        }
        projects.push(project(name, vec![record]));
    }
    let reader = FakeRegistry::new(enumeration(projects, &scratch));
    let revision = reader
        .snapshot_scoped("module", &RegistryScope::all())
        .await
        .unwrap();
    assert!(revision.project_layers("healthy").is_ok());
    for (name, code) in cases {
        let layers = revision.project_layers(name);
        if name == "at-limit" {
            let bytes = &layers.unwrap().project.unwrap().1.bytes;
            assert_eq!(bytes.len(), 1024 * 1024);
            assert_eq!(
                crate::config_gate::capture::parse_layer(bytes).unwrap(),
                json!({})
            );
        } else {
            let error = layers.err().unwrap();
            assert_eq!(error.code, code, "{name}: {error}");
            match name {
                "linked-root" => assert_eq!(
                    error.detail,
                    "root open refused: input leaf is a symlink or reparse point"
                ),
                "linked-component" | "linked-leaf" => {
                    assert_eq!(error.detail, "entry is a symlink or reparse point")
                }
                _ => {}
            }
        }
    }
}

#[cfg(windows)]
#[tokio::test]
async fn registry_refuses_reparse_roots_and_layer_components() {
    use std::{fs, process::Command};
    let scratch = RegistryScratch::new("registry-reparse");
    let target = scratch.join("target");
    let linked_root = scratch.join("linked-root");
    let linked_config = scratch.join("linked-config");
    fs::create_dir(&target).unwrap();
    fs::create_dir(&linked_config).unwrap();
    let junction = |path: &Path| {
        assert!(Command::new("cmd")
            .args(["/d", "/c", "mklink", "/j"])
            .arg(path)
            .arg(&target)
            .status()
            .unwrap()
            .success());
    };
    junction(&linked_root);
    junction(&linked_config.join(".cortexkit"));
    let mut replaced = root(&target);
    replaced["root"] = json!(linked_root);
    let reader = FakeRegistry::new(enumeration(
        vec![
            project("root", vec![replaced]),
            project("config", vec![root(&linked_config)]),
            project("healthy", vec![]),
        ],
        &scratch,
    ));
    let revision = reader
        .snapshot_scoped("module", &RegistryScope::all())
        .await
        .unwrap();
    for id in ["root", "config"] {
        let error = revision.project_layers(id).err().unwrap();
        assert_eq!(error.code, "layer_symlink", "{id}: {error}");
    }
    assert!(revision.project_layers("healthy").is_ok());
}

#[cfg(unix)]
#[tokio::test]
async fn registry_recheck_refuses_new_file_swapped_directory_or_renamed_root() {
    use std::fs;
    let scratch = RegistryScratch::new("registry-recheck-location");
    let root_path = scratch.join("root");
    let other = scratch.join("other");
    let path = layer_path(&root_path, "module");
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent).unwrap();
    fs::create_dir_all(other.join(".cortexkit")).unwrap();
    let mut root_record = root(&root_path);
    root_record["fileId"] = Value::Null;
    let reader = FakeRegistry::new(enumeration(
        vec![project("project", vec![root_record, root(&other)])],
        &scratch,
    ));
    let scope = RegistryScope::new(
        BTreeSet::new(),
        BTreeSet::from([path.clone()]),
        BTreeSet::new(),
    );
    let saved = reader.snapshot_scoped("module", &scope).await.unwrap();
    fs::write(layer_path(&other, "module"), b"{} unrelated dependency").unwrap();
    reader.recheck(&saved, &scope).await.unwrap();
    fs::write(&path, b"{}").unwrap();
    assert_eq!(
        reader.recheck(&saved, &scope).await.unwrap_err().code,
        "config_changed_since_review"
    );
    fs::remove_file(&path).unwrap();
    fs::rename(parent, root_path.join("old-config")).unwrap();
    fs::create_dir(parent).unwrap();
    assert_eq!(
        reader.recheck(&saved, &scope).await.unwrap_err().code,
        "config_changed_since_review"
    );
    fs::remove_dir(parent).unwrap();
    fs::rename(root_path.join("old-config"), parent).unwrap();
    reader.recheck(&saved, &scope).await.unwrap();
    fs::rename(&root_path, scratch.join("renamed-root")).unwrap();
    assert_eq!(
        reader.recheck(&saved, &scope).await.unwrap_err().code,
        "config_changed_since_review"
    );
    fs::create_dir(&root_path).unwrap();
    assert_eq!(
        reader.recheck(&saved, &scope).await.unwrap_err().code,
        "config_changed_since_review"
    );
}
