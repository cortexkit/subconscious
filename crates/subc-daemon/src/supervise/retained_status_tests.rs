use super::*;

#[tokio::test]
async fn cached_container_block_is_visible_in_status_and_health_without_rechecking() {
    use crate::live_children::{ContainerIdentity, RetainedReason};
    for (reason, carried, expected) in [
        (
            RetainedReason::ContainerSurvivors(vec![4242]),
            true,
            Some("waiting: container survivors (pids [4242])".to_owned()),
        ),
        (
            RetainedReason::ContainerUnverifiable("member inspection denied".into()),
            true,
            Some(
                "waiting: container survivors could not be verified: member inspection denied"
                    .to_owned(),
            ),
        ),
        (RetainedReason::Survived, false, None),
    ] {
        let supervisor =
            Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
        let spec = ModuleSpec {
            script: None,
            module_id: "retained-status-fixture".into(),
            program: PathBuf::from("/not-spawned"),
            args: Vec::new(),
            env: Vec::new(),
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::None,
            overlap: ModuleOverlap::Exclusive,
            carry_containment: carried,
        };
        let module = supervisor
            .supervise_configured(spec.clone(), false)
            .unwrap();
        // No process is launched. Invalid group zero is deliberate: if a
        // status read wrongly rechecks, verification changes the reason
        // and fails the assertions without signalling any real process.
        let guard = supervisor
            .child_roster
            .admit(
                spec.module_id,
                0,
                ModuleProtocol::None,
                None,
                crate::child_roster::RecordedIdentity {
                    container: carried.then_some(ContainerIdentity {
                        pgid: Some(0),
                        created_start_time: Some(0),
                        cgroup_path: None,
                        ..ContainerIdentity::test_metadata()
                    }),
                    ..crate::child_roster::RecordedIdentity::default()
                },
            )
            .unwrap();
        guard.retain(reason);
        let status = module.status().unwrap();
        assert_eq!(status.waiting_for_previous_detail, expected.clone());
        assert_eq!(status.health.detail, expected);
        assert!(!status.process_alive);
    }
}
