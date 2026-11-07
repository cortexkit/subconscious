use super::*;
use cortexkit_test_support::ScratchDir;

#[test]
fn version_two_round_trips_the_complete_container_identity() {
    let dir = ScratchDir::new("container-record-v2");
    let path = record_path(&dir);
    let entry = LiveChild {
        module_id: "round-trip".into(),
        pid: 42,
        protocol: ModuleProtocol::None,
        start_time: Some(100),
        executable: None,
        cgroup_name: None,
        container: Some(ContainerIdentity {
            pgid: Some(42),
            created_start_time: Some(100),
            boot_id: Some("11111111-2222-3333-4444-555555555555".into()),
            session_id: Some(77),
            cgroup_path: None,
        }),
    };
    write_record(&path, std::slice::from_ref(&entry)).unwrap();
    let wire: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        wire["version"], 2,
        "older readers must reject the new format"
    );
    assert_eq!(
        wire["children"][0]["container"]["boot_id"],
        "11111111-2222-3333-4444-555555555555"
    );
    assert_eq!(wire["children"][0]["container"]["session_id"], 77);
    assert_eq!(read_record(&path).unwrap(), vec![entry]);
}

#[cfg(unix)]
#[tokio::test]
async fn version_one_carried_entry_without_boot_or_session_blocks_only_its_module_without_signalling(
) {
    use std::os::unix::process::CommandExt;

    struct Owned(std::process::Child);
    impl Drop for Owned {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let dir = ScratchDir::new("legacy-container-without-boot");
    let path = record_path(&dir);
    let mut child = Owned(
        std::process::Command::new("/bin/sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .unwrap(),
    );
    let pid = child.0.id();
    let birth = subc_os::process_group::inspect(pid)
        .unwrap()
        .unwrap()
        .start_time;
    fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "children": [{
                "module_id": "legacy-carried",
                "pid": pid,
                "protocol": "none",
                "start_time": birth,
                "executable": null,
                "cgroup_name": null,
                "container": {
                    "pgid": pid,
                    "created_start_time": birth,
                    "cgroup_path": null
                }
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let parsed = read_record(&path).expect("valid v1 remains readable");
    let identity = parsed[0].container.as_ref().unwrap();
    assert!(identity.boot_id.is_none() && identity.session_id.is_none());
    let owner = RunDirLock::acquire(&path).unwrap();
    let decisions = sweep_orphans(&owner, &AdoptedPids::none(), SweepBounds::default()).await;
    assert!(
        matches!(&decisions[..], [(_, SweepDecision::Unverifiable(_))]),
        "missing boot identity is unknown, never Gone or a signal authorization: {decisions:?}"
    );
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "the owned legacy leader must not be signalled"
    );
    let roster = crate::child_roster::ChildRoster::default();
    roster.record_to(path);
    assert!(
        matches!(roster.previous_instance("legacy-carried"), Some((found, RetainedReason::ContainerUnverifiable(_))) if found == pid)
    );
    assert_eq!(roster.previous_instance("unrelated"), None);
    assert!(
        roster.recovery_block().is_none(),
        "a valid legacy entry must not globally quarantine unrelated modules"
    );
    assert!(child.0.try_wait().unwrap().is_none());
}
