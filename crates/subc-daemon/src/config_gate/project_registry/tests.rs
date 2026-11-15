use super::*;
use cortexkit_test_support::ScratchDir;
use serde_json::{json, Value};

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
    serde_json::from_value(json!({"projects":projects,"workspaces":[{"workspaceId":"workspace","name":"Workspace","root":workspace,"fileId":file_id(workspace)}],
        "generation":7,"incarnation":"registry-process"})).unwrap()
}

fn snapshot(reply: &EnumerateReply) -> Result<RegistryRevision> {
    RegistryRevision::from_enumerate(reply, "module", &BTreeSet::from(["project".into()]))
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

#[test]
fn registry_snapshot_workspace_then_project_from_fake_enumerate() {
    let dir = ScratchDir::new("registry-snapshot");
    let workspace = dir.join("workspace");
    let local = dir.join("project");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&local).unwrap();
    let mut reply = enumeration(vec![project("project", vec![])], &workspace);
    let saved = snapshot(&reply).unwrap();
    assert!(saved.projects[0].project_roots.is_empty());
    assert_eq!(
        saved.projects[0].workspace_root.as_ref().unwrap().path,
        workspace
    );
    reply.projects[0] = serde_json::from_value(project("project", vec![root(&local)])).unwrap();
    let saved = snapshot(&reply).unwrap();
    assert_eq!(saved.projects[0].project_roots[0].path, local);
    assert_eq!(
        saved.projects[0].workspace_root.as_ref().unwrap().path,
        workspace
    );
}

#[test]
fn registry_root_selection_zero_ineligible_one_and_multiple_candidates() {
    let dir = ScratchDir::new("registry-candidates");
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
    assert_eq!(error.code, "project_registry_unavailable");
    assert!(error.detail.contains(first.to_str().unwrap()));
    assert!(error.detail.contains(second.to_str().unwrap()));
}

#[test]
fn registry_identity_absent_malformed_mismatched_and_incarnation_only() {
    let dir = ScratchDir::new("registry-identities");
    let mut reply = enumeration(vec![project("project", vec![root(&dir)])], &dir);
    reply.workspaces[0].file_id = None;
    let error = snapshot(&reply).unwrap_err();
    assert_eq!(error.code, "project_registry_unavailable");
    assert!(error.detail.contains("project"));
    reply.workspaces[0].file_id = Some(file_id(&dir));
    for bad in ["broken", "unix:0:0"] {
        reply.projects[0].root_records.as_mut().unwrap()[0].file_id = Some(bad.into());
        let error = snapshot(&reply).unwrap_err();
        assert_eq!(error.code, "project_registry_unavailable");
        assert!(error.detail.contains("project"));
    }
    let record = &mut reply.projects[0].root_records.as_mut().unwrap()[0];
    record.file_id = None;
    assert!(!snapshot(&reply).unwrap().projects[0]
        .project_roots
        .is_empty());
    reply.projects[0].root_records.as_mut().unwrap()[0].incarnation = None;
    assert!(snapshot(&reply).unwrap().projects[0]
        .project_roots
        .is_empty());
}

#[test]
fn registry_commit_recheck_ignores_unrelated_edits_but_refuses_affected_changes() {
    let dir = ScratchDir::new("registry-recheck");
    let reply = enumeration(
        vec![
            project("project", vec![root(&dir)]),
            project("unrelated", vec![]),
        ],
        &dir,
    );
    let saved = snapshot(&reply).unwrap();
    let mut unrelated = reply.clone();
    unrelated.generation += 1;
    unrelated.incarnation = "another-registry-process".into();
    unrelated.projects[1].workspace_id = None;
    let current = snapshot(&unrelated).unwrap();
    saved.recheck(&current).unwrap();
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
            saved.recheck(&current).unwrap_err().code,
            "config_changed_since_review",
            "{change}"
        );
    }
}

#[tokio::test]
async fn registry_recheck_rereads_only_the_saved_projects() {
    struct FakeRegistry(std::sync::Mutex<EnumerateReply>);
    impl ProjectRegistry for FakeRegistry {
        fn snapshot<'a>(
            &'a self,
            module: &'a str,
            affected: &'a BTreeSet<String>,
        ) -> RegistryFuture<'a> {
            Box::pin(async move {
                RegistryRevision::from_enumerate(&self.0.lock().unwrap(), module, affected)
            })
        }
    }
    let dir = ScratchDir::new("registry-reread");
    let reply = enumeration(
        vec![
            project("project", vec![root(&dir)]),
            project("unrelated", vec![]),
        ],
        &dir,
    );
    let reader = FakeRegistry(std::sync::Mutex::new(reply));
    let saved = reader
        .snapshot("module", &BTreeSet::from(["project".into()]))
        .await
        .unwrap();
    reader.0.lock().unwrap().projects[1].project_id = "unrelated-renamed".into();
    reader.recheck(&saved).await.unwrap();
    reader.0.lock().unwrap().projects[0].workspace_id = None;
    assert_eq!(
        reader.recheck(&saved).await.unwrap_err().code,
        "config_changed_since_review"
    );
    reader.0.lock().unwrap().projects[0]
        .root_records
        .as_mut()
        .unwrap()[0]
        .file_id = Some("unix:0:0".into());
    assert_eq!(
        reader.recheck(&saved).await.unwrap_err().code,
        "config_changed_since_review"
    );
}

#[cfg(windows)]
#[test]
fn registry_windows_real_directory_file_index_matches_and_mismatch_refuses() {
    let dir = ScratchDir::new("registry-windows-index");
    let file = subc_os::open_directory_nofollow_all(&dir).unwrap();
    let identity = RegistryFileId::parse(&file_id(&dir)).unwrap();
    assert!(identity.matches(&file).unwrap());
    let RegistryFileId::Windows {
        volume_serial,
        file_index,
    } = identity
    else {
        panic!("not Windows")
    };
    assert!(!RegistryFileId::Windows {
        volume_serial,
        file_index: file_index ^ 1
    }
    .matches(&file)
    .unwrap());
}
