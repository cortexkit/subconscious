use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use std::fmt::Debug;
use subc_control::{
    AgentCommandResources, ChildMemoryKind, ChildResourceReading, ChildResourceUsage, CpuLastHour,
    DaemonResources, LaunchSpecAgreement, PendingReloadVerdict, ProcessGroup, ReloadPathAgreement,
    ResourceBound, RunningImageAgreement, RunningImageUnavailableReason, TreeOwn, TreeReading,
    TreeSource,
};

fn assert_wire<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T, wire: Value) {
    assert_eq!(serde_json::to_value(value).unwrap(), wire);
    assert_eq!(&serde_json::from_value::<T>(wire).unwrap(), value);
}

#[test]
fn resource_constructors_preserve_required_wire_fields_and_optional_defaults() {
    assert_wire(
        &ProcessGroup::new("worker", 2, 3, 4),
        json!({"name":"worker","count":2,"cpu_ms":3,"memory_bytes":4}),
    );
    assert_wire(&TreeOwn::new(3, 4), json!({"cpu_ms":3,"memory_bytes":4}));
    assert_wire(
        &TreeReading::new(
            3,
            4,
            ChildMemoryKind::ResidentSet,
            2,
            TreeSource::ProcessTree,
        ),
        json!({"cpu_ms":3,"memory_bytes":4,"memory_kind":"resident_set","process_count":2,"source":"process_tree","groups":[]}),
    );
    assert_wire(
        &CpuLastHour::new(3, 60_000),
        json!({"cpu_ms":3,"covered_ms":60_000}),
    );
    assert_wire(
        &DaemonResources::new(7, 8, "version"),
        json!({"pid":7,"start_time_ms":8,"version":"version"}),
    );
    assert_wire(
        &AgentCommandResources::new("module", 3, 4, 2),
        json!({"module_id":"module","cpu_ms":3,"memory_bytes":4,"process_count":2,"groups":[]}),
    );
    assert_wire(
        &ChildResourceReading::new(4, ChildMemoryKind::ResidentSet, 1, 2),
        json!({"memory_bytes":4,"memory_kind":"resident_set","cpu_user_ms":1,"cpu_system_ms":2}),
    );
    assert_wire(
        &PendingReloadVerdict::new(
            ReloadPathAgreement::Match,
            RunningImageAgreement::Unavailable {
                reason: RunningImageUnavailableReason::NotRunning,
            },
        ),
        json!({"path":{"status":"match"},"image":{"status":"unavailable","reason":"not_running"}}),
    );
}

#[test]
fn resource_builders_set_and_clear_all_optional_metadata() {
    let lower = Some(ResourceBound::Lower);
    let group = ProcessGroup::new("worker", 2, 3, 4).with_bound(lower.clone());
    assert_wire(
        &group,
        json!({"name":"worker","count":2,"cpu_ms":3,"memory_bytes":4,"bound":"lower"}),
    );
    assert_eq!(
        group.clone().with_bound(None),
        ProcessGroup::new("worker", 2, 3, 4)
    );
    let own = TreeOwn::new(1, 2).with_bound(lower.clone());
    assert_wire(&own, json!({"cpu_ms":1,"memory_bytes":2,"bound":"lower"}));
    assert_eq!(own.clone().with_bound(None), TreeOwn::new(1, 2));
    let hour = CpuLastHour::new(3, 60_000).with_bound(lower.clone());
    assert_wire(
        &hour,
        json!({"cpu_ms":3,"covered_ms":60_000,"bound":"lower"}),
    );
    assert_eq!(hour.clone().with_bound(None), CpuLastHour::new(3, 60_000));

    let tree = TreeReading::new(
        3,
        4,
        ChildMemoryKind::ResidentSet,
        2,
        TreeSource::ProcessTree,
    );
    let populated = tree
        .clone()
        .with_own(Some(own))
        .with_groups(vec![group.clone()])
        .with_bound(lower.clone());
    assert_eq!(populated.own.as_ref().unwrap().cpu_ms, 1);
    assert_eq!(populated.groups, vec![group.clone()]);
    assert_eq!(populated.bound, lower);
    assert_eq!(
        populated
            .with_own(None)
            .with_groups(Vec::new())
            .with_bound(None),
        tree
    );

    let process =
        ChildResourceReading::new(4, ChildMemoryKind::ResidentSet, 1, 2).with_swap_bytes(Some(5));
    assert_wire(
        &process,
        json!({"memory_bytes":4,"memory_kind":"resident_set","cpu_user_ms":1,"cpu_system_ms":2,"swap_bytes":5}),
    );
    assert_eq!(
        process.clone().with_swap_bytes(None),
        ChildResourceReading::new(4, ChildMemoryKind::ResidentSet, 1, 2)
    );

    let daemon = DaemonResources::new(7, 8, "version");
    let resources = Some(ChildResourceUsage::Measured(process));
    let populated = daemon
        .clone()
        .with_resources(resources.clone())
        .with_cpu_last_hour(Some(hour.clone()));
    assert_eq!(populated.resources, resources);
    assert_eq!(populated.cpu_last_hour, Some(hour.clone()));
    assert_eq!(
        populated.with_resources(None).with_cpu_last_hour(None),
        daemon
    );

    let agents = AgentCommandResources::new("module", 3, 4, 2);
    let populated = agents
        .clone()
        .with_bound(lower.clone())
        .with_groups(vec![group.clone()])
        .with_unmeasured_finished(Some(5))
        .with_cpu_last_hour(Some(hour.clone()));
    assert_eq!(populated.bound, lower);
    assert_eq!(populated.groups, vec![group]);
    assert_eq!(populated.unmeasured_finished, Some(5));
    assert_eq!(populated.cpu_last_hour, Some(hour));
    assert_eq!(
        populated
            .with_bound(None)
            .with_groups(Vec::new())
            .with_unmeasured_finished(None)
            .with_cpu_last_hour(None),
        agents
    );

    let verdict = PendingReloadVerdict::new(
        ReloadPathAgreement::Match,
        RunningImageAgreement::Unavailable {
            reason: RunningImageUnavailableReason::NotRunning,
        },
    );
    let populated = verdict.clone().with_spec(Some(LaunchSpecAgreement::Match));
    assert_eq!(populated.spec, Some(LaunchSpecAgreement::Match));
    assert_eq!(populated.with_spec(None), verdict);
}
