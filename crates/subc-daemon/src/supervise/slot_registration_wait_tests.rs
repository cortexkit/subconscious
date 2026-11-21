use super::*;
use crate::registry::{ConnectionId, RegistrationSlot};
use subc_protocol::manifest::ModuleManifest;

#[tokio::test]
async fn enable_release_failure_is_failed_and_a_second_enable_retries() {
    let registry = Arc::new(Registry::default());
    let supervisor = Supervisor::new_for_test(Arc::clone(&registry), RestartPolicy::default());
    let runtime = supervisor.runtime_config();
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::disabled()));
    let spec = ModuleSpec {
        script: None,
        module_id: "enable-stale-registration".to_string(),
        program: PathBuf::from("/missing/enable-retry-test"),
        args: Vec::new(),
        env: Vec::new(),
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    };
    let connection = ConnectionId::new(90);
    registry
        .register_with_control_ops(
            ModuleManifest::builder(&spec.module_id, "0.1.0").build(),
            1,
            connection,
            Vec::new(),
        )
        .unwrap();
    let mut child = None;
    let err = set_child_enabled(
        &spec,
        &runtime,
        &registry,
        &supervisor.process_liveness,
        &snapshot,
        &mut child,
        true,
        &runtime.failure_context(false),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        SuperviseError::RegistrationStillActive { .. }
    ));
    assert_eq!(lock_snapshot(&snapshot).unwrap().state, ModuleState::Failed);
    assert!(child.is_none());
    registry.deregister_connection(connection).unwrap();
    let err = set_child_enabled(
        &spec,
        &runtime,
        &registry,
        &supervisor.process_liveness,
        &snapshot,
        &mut child,
        true,
        &runtime.failure_context(false),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, SuperviseError::Spawn { .. }),
        "second enable must attempt a spawn: {err}"
    );
    assert_eq!(lock_snapshot(&snapshot).unwrap().state, ModuleState::Failed);
}

const INCUMBENT: u64 = 1;
const CANDIDATE: u64 = 2;

fn swapped_registry() -> Arc<Registry> {
    let registry = Arc::new(Registry::default());
    let manifest = ModuleManifest::builder("m", "0.1.0").build();
    registry
        .register_with_control_ops(
            manifest.clone(),
            1,
            ConnectionId::new(INCUMBENT),
            Vec::new(),
        )
        .unwrap();
    registry
        .register_candidate_with_control_ops(manifest, 1, ConnectionId::new(CANDIDATE), Vec::new())
        .unwrap();
    registry
}

/// After a promotion the id's active slot is held by the new process, so an
/// id-keyed wait for the incumbent's release can never succeed; the
/// connection-keyed wait completes as soon as the incumbent deregisters.
#[tokio::test]
async fn incumbent_release_is_awaited_by_connection_not_by_module_id() {
    let registry = swapped_registry();
    registry.promote_candidate("m").unwrap().unwrap();

    assert!(matches!(
        wait_for_registration_release(&registry, "m", Duration::from_millis(50)).await,
        Err(SuperviseError::RegistrationStillActive { .. })
    ));

    // Still held while the incumbent's connection has not deregistered.
    assert!(matches!(
        wait_for_slot_registration_release(
            &registry,
            RegistrationSlot::Connection(ConnectionId::new(INCUMBENT)),
            Duration::from_millis(50),
        )
        .await,
        Err(SuperviseError::RegistrationStillActive { .. })
    ));

    let releaser = Arc::clone(&registry);
    let release = tokio::spawn(async move {
        sleep(Duration::from_millis(20)).await;
        releaser
            .deregister_connection(ConnectionId::new(INCUMBENT))
            .unwrap();
        notify_registration_release();
    });
    wait_for_slot_registration_release(
        &registry,
        RegistrationSlot::Connection(ConnectionId::new(INCUMBENT)),
        Duration::from_secs(5),
    )
    .await
    .expect("the incumbent's own registration is released");
    release.await.unwrap();
    assert!(registry.get_module("m").unwrap().is_some());
}

/// The candidate slot is waited on separately from the active slot: the
/// incumbent's registration neither holds up nor stands in for it.
#[tokio::test]
async fn candidate_slot_wait_ignores_the_incumbents_registration() {
    let registry = swapped_registry();
    assert!(matches!(
        wait_for_slot_registration_release(
            &registry,
            RegistrationSlot::Candidate("m"),
            Duration::from_millis(50),
        )
        .await,
        Err(SuperviseError::RegistrationStillActive { .. })
    ));
    registry
        .deregister_connection(ConnectionId::new(CANDIDATE))
        .unwrap();
    wait_for_slot_registration_release(
        &registry,
        RegistrationSlot::Candidate("m"),
        Duration::from_millis(50),
    )
    .await
    .expect("a candidate slot with no candidate is released");
    assert!(registry
        .registration(RegistrationSlot::Active("m"))
        .unwrap()
        .is_some());
}
