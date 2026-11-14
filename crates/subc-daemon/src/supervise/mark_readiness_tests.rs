//! The supervisor's half of the first-party mark: a launch's authority record
//! reaches its configuration delivery spawn, and a healthy probe counts as the
//! spawn reporting ready only when the probed registration is the spawn's own.

use super::*;
use crate::spawn_authority::{MarkStatus, SpawnAuthorityRecord};
use subc_protocol::peer::CallerOrigin;

const OWN: ConnectionId = ConnectionId::new(40);

fn healthy() -> HealthReport {
    HealthReport {
        status: HealthStatus::Ok,
        detail: None,
        metrics: None,
    }
}

#[tokio::test]
async fn first_party_mark_healthy_probe_marks_only_the_childs_own_registration() {
    let (_rig, _owner, supervisor, spec, acknowledge) =
        crate::config_gate::runtime::RuntimeOwner::mark_supervisor_fixture_for_test();
    // The native fixture's first argument selects its mode; the next is the
    // seen marker that keeps this child running instead of exiting at once.
    assert_eq!(spec.args[0], "mark-first-exit23");
    std::fs::write(&spec.args[1], b"").unwrap();
    let forwarding = Arc::new(crate::forwarding::ForwardingTable::default());
    let mut runtime = supervisor.runtime_config();
    runtime.forwarding = Some(forwarding.clone());
    let registry = runtime.registry.clone();
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    let mut child = Some(
        spawn_and_mark_running_async(&spec, &runtime, &snapshot)
            .await
            .unwrap(),
    );

    // The launch's authority record reached its delivery spawn, admitted under
    // the stamp.
    let launched = child.as_ref().unwrap();
    let spawn = launched
        .config_delivery
        .clone()
        .expect("a gated launch has a delivery spawn");
    let record = spawn
        .authority()
        .cloned()
        .expect("the launch's authority record reaches its delivery spawn");
    assert!(Arc::ptr_eq(
        &record,
        &launched.registration_spawn.as_ref().unwrap().authority
    ));
    assert!(spawn.stamped());
    record.set_origin_for_test(CallerOrigin::Release);

    // The module's active registration, which health probes reach.
    let (sink, _module_rx) = tokio::sync::mpsc::channel(8);
    forwarding
        .register_module_connection(
            OWN,
            spec.module_id.clone(),
            subc_protocol::PROTOCOL_VERSION,
            subc_protocol::manifest::Concurrency::ModuleManaged,
            crate::FrameSink::new(sink),
        )
        .unwrap();
    registry
        .register_with_control_ops(
            subc_protocol::manifest::ModuleManifest::builder(&spec.module_id, "1.0.0").build(),
            subc_protocol::PROTOCOL_VERSION,
            OWN,
            Vec::new(),
        )
        .unwrap();
    acknowledge(OWN, &spawn);
    assert_eq!(record.mark_status(), MarkStatus::AwaitingReadiness);

    macro_rules! probe {
        () => {
            handle_health_report(
                &spec,
                &runtime,
                &registry,
                &supervisor.process_liveness,
                &snapshot,
                &mut child,
                healthy(),
                1,
                None,
            )
            .await
        };
    }
    // While the active registration belongs to another spawn (an old spawn
    // still registered, or a candidate promoted over this child), its healthy
    // answer is not this child reporting ready.
    for other in ["old spawn", "promoted candidate"] {
        forwarding
            .bind_target_authority(OWN, SpawnAuthorityRecord::new(other, 99, &spec.module_id))
            .unwrap();
        probe!();
        assert!(!record.first_party(), "{other}");
        assert_eq!(
            record.mark_status(),
            MarkStatus::AwaitingReadiness,
            "{other}"
        );
    }
    // The child's own registration answering healthy marks it.
    forwarding
        .bind_target_authority(OWN, record.clone())
        .unwrap();
    probe!();
    assert!(record.first_party(), "{:?}", record.mark_status());
}
