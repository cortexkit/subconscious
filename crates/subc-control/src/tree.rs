use serde::{Deserialize, Serialize};

open_string_enum! {
    /// The measurement behind cumulative tree resources.
    #[non_exhaustive]
    TreeSource {
        Cgroup => "cgroup",
        Job => "job",
        ProcessTree => "process_tree"
    }
}

open_string_enum! {
    /// Exact counters omit this member; uncontained or ambiguous CPU is a floor.
    #[non_exhaustive]
    ResourceBound {
        Lower => "lower"
    }
}

/// A live program group, ordered by descending memory in a tree reading.
/// CPU is funded attributed credit of its currently live identities; the row
/// additionally retains retired work. Groups never exceed their row's CPU.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessGroup {
    pub name: String,
    pub count: u64,
    pub cpu_ms: u64,
    pub memory_bytes: u64,
    /// A live-only attributed CPU floor; absent means exact. Memory is not a floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound: Option<ResourceBound>,
}

/// Cumulative CPU and current memory of a module and its descendants.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TreeReading {
    pub cpu_ms: u64,
    pub memory_bytes: u64,
    pub memory_kind: crate::ChildMemoryKind,
    pub process_count: u64,
    pub source: TreeSource,
    /// Absent means exact. Memory and process count are not CPU bounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound: Option<ResourceBound>,
    /// Absent means no program groups. Unreadable members invalidate the whole
    /// reading instead of being represented by an empty collection.
    #[serde(default)]
    pub groups: Vec<ProcessGroup>,
}

/// CPU used between the retained wall-clock samples, not a percentage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CpuLastHour {
    pub cpu_ms: u64,
    pub covered_ms: u64,
    /// Only the covered hour is a floor; cumulative CPU has a separate bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound: Option<ResourceBound>,
}

/// The daemon's own process, excluding CPU of its reaped module children.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonResources {
    pub pid: u32,
    pub start_time_ms: u64,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<crate::ChildResourceUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_last_hour: Option<CpuLastHour>,
}

/// Commands attributed to agents rather than to the module that launches them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentCommandResources {
    pub module_id: String,
    pub cpu_ms: u64,
    pub memory_bytes: u64,
    pub process_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound: Option<ResourceBound>,
    /// Accepted commands that finished before any successful CPU measurement.
    /// Their CPU remains in the module row, not a fabricated agents zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unmeasured_finished: Option<u64>,
    /// Absent means no live program groups, never unreadable members.
    #[serde(default)]
    pub groups: Vec<ProcessGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_last_hour: Option<CpuLastHour>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hour_bound_is_optional_and_independent_of_cumulative_bounds() {
        let old: CpuLastHour =
            serde_json::from_value(serde_json::json!({"cpu_ms":0,"covered_ms":60_000})).unwrap();
        assert_eq!(old.bound, None);
        assert!(serde_json::to_value(old).unwrap().get("bound").is_none());
        let lower: CpuLastHour = serde_json::from_value(
            serde_json::json!({"cpu_ms":0,"covered_ms":60_000,"bound":"lower","future":true}),
        )
        .unwrap();
        assert_eq!(lower.bound, Some(ResourceBound::Lower));
        assert_eq!(serde_json::to_value(lower).unwrap()["bound"], "lower");
    }
    use crate::{ClientControlResponse, SupervisorEntry, SupervisorHealthStatus};

    #[test]
    fn resource_totals_round_trip_and_ignore_unknown_members() {
        let reading = TreeReading {
            bound: None,
            cpu_ms: 123,
            memory_bytes: 456,
            memory_kind: crate::ChildMemoryKind::ResidentSet,
            process_count: 2,
            source: TreeSource::ProcessTree,
            groups: vec![ProcessGroup {
                bound: None,
                name: "compiler".into(),
                count: 1,
                cpu_ms: 100,
                memory_bytes: 400,
            }],
        };
        let mut module =
            SupervisorEntry::new("module", "running", true, true, SupervisorHealthStatus::Ok)
                .with_tree(Some(reading));
        module.cpu_last_hour = Some(CpuLastHour {
            cpu_ms: 120,
            covered_ms: 60_000,
            bound: Some(ResourceBound::Lower),
        });
        let response = ClientControlResponse::SupervisorList {
            generation: 1,
            modules: vec![module],
            daemon: Some(DaemonResources {
                pid: 1,
                start_time_ms: 1_700_000_000_000,
                version: "1.0.0".into(),
                resources: None,
                cpu_last_hour: Some(CpuLastHour {
                    cpu_ms: 5,
                    covered_ms: 60_000,
                    bound: None,
                }),
            }),
            agent_commands: Some(vec![AgentCommandResources {
                module_id: "module".into(),
                bound: None,
                unmeasured_finished: None,
                cpu_ms: 300,
                memory_bytes: 100,
                process_count: 1,
                groups: Vec::new(),
                cpu_last_hour: None,
            }]),
        };
        let mut value = serde_json::to_value(&response).unwrap();
        assert!(value["modules"][0].get("subtree").is_none());
        value["future"] = serde_json::json!({"anything":true});
        value["modules"][0]["future"] = serde_json::json!(1);
        value["modules"][0]["tree"]["future"] = serde_json::json!(2);
        value["modules"][0]["tree"]["groups"][0]["future"] = serde_json::json!(3);
        value["daemon"]["future"] = serde_json::json!(4);
        value["agent_commands"][0]["future"] = serde_json::json!(5);
        let decoded: ClientControlResponse = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, response);
        let ClientControlResponse::SupervisorList { modules, .. } = decoded else {
            panic!("wrong response")
        };
        assert_eq!(modules[0].tree.as_ref().unwrap().cpu_ms, 123);
        assert_eq!(
            modules[0].cpu_last_hour.as_ref().unwrap().covered_ms,
            60_000
        );
    }

    #[test]
    fn unknown_resource_totals_are_omitted_and_legacy_lists_decode() {
        let value = serde_json::json!({"op":"supervisor.list","generation":1,"modules":[{
            "module_id":"module","state":"stopped","enabled":false,"live":false,"health":"unknown"
        }]});
        let response: ClientControlResponse = serde_json::from_value(value).unwrap();
        let encoded = serde_json::to_value(response).unwrap();
        assert!(encoded.get("daemon").is_none());
        assert!(encoded.get("agent_commands").is_none());
        assert!(encoded["modules"][0].get("tree").is_none());
        assert!(encoded["modules"][0].get("tree_unavailable").is_none());
        assert!(encoded["modules"][0].get("cpu_last_hour").is_none());
    }

    #[test]
    fn tree_unavailable_is_additive_and_preserves_unknown_reasons() {
        #[derive(Deserialize)]
        struct OlderEntry {
            module_id: String,
            #[serde(default)]
            tree: Option<TreeReading>,
        }

        for reason in ["not_yet_measured", "future_reason"] {
            let entry = SupervisorEntry::new(
                "module",
                "running",
                true,
                true,
                SupervisorHealthStatus::Unknown,
            )
            .with_tree_unavailable(Some(reason.to_owned()));
            let bytes = serde_json::to_vec(&entry).unwrap();
            assert_eq!(
                String::from_utf8(bytes.clone()).unwrap(),
                format!(
                    r#"{{"module_id":"module","state":"running","enabled":true,"live":true,"protocol":"subc","health":"unknown","last_probe_ms":null,"tree_unavailable":"{reason}"}}"#
                )
            );
            assert_eq!(
                serde_json::from_slice::<SupervisorEntry>(&bytes).unwrap(),
                entry
            );
            let older: OlderEntry = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(older.module_id, "module");
            assert!(older.tree.is_none());
        }
    }
}

#[cfg(test)]
mod optional_member_tests {
    use super::*;
    use serde::de::DeserializeOwned;
    use serde_json::{json, Value};

    fn refuses_missing<T: DeserializeOwned>(base: &Value, fields: &[&str]) {
        for field in fields {
            let mut missing = base.clone();
            missing.as_object_mut().unwrap().remove(*field);
            assert!(
                serde_json::from_value::<T>(missing).is_err(),
                "missing {field} must not become zero"
            );
        }
    }

    #[test]
    fn nested_resource_types_default_only_optional_members_and_collections() {
        let tree = json!({"cpu_ms":1,"memory_bytes":2,"memory_kind":"resident_set","process_count":3,"source":"cgroup"});
        let reading: TreeReading = serde_json::from_value(tree.clone()).unwrap();
        assert!(reading.groups.is_empty());
        assert!(reading.bound.is_none());
        refuses_missing::<TreeReading>(
            &tree,
            &[
                "cpu_ms",
                "memory_bytes",
                "memory_kind",
                "process_count",
                "source",
            ],
        );

        let agents = json!({"module_id":"compiler","cpu_ms":1,"memory_bytes":2,"process_count":3});
        let reading: AgentCommandResources = serde_json::from_value(agents.clone()).unwrap();
        assert!(reading.groups.is_empty());
        assert!(reading.bound.is_none());
        assert!(reading.unmeasured_finished.is_none());
        assert!(reading.cpu_last_hour.is_none());
        refuses_missing::<AgentCommandResources>(
            &agents,
            &["module_id", "cpu_ms", "memory_bytes", "process_count"],
        );

        let daemon = json!({"pid":1,"start_time_ms":2,"version":"3"});
        let reading: DaemonResources = serde_json::from_value(daemon.clone()).unwrap();
        assert!(reading.resources.is_none());
        assert!(reading.cpu_last_hour.is_none());
        refuses_missing::<DaemonResources>(&daemon, &["pid", "start_time_ms", "version"]);

        // Optional minimum metadata does not make any measurement optional.
        let group = json!({"name":"compiler","count":1,"cpu_ms":2,"memory_bytes":3});
        assert!(serde_json::from_value::<ProcessGroup>(group.clone())
            .unwrap()
            .bound
            .is_none());
        refuses_missing::<ProcessGroup>(&group, &["name", "count", "cpu_ms", "memory_bytes"]);
    }

    #[test]
    fn lower_bounds_and_unmeasured_finishes_round_trip_additively() {
        let tree = json!({"cpu_ms":1,"memory_bytes":2,"memory_kind":"resident_set","process_count":3,"source":"process_tree","bound":"lower","future":true});
        let reading: TreeReading = serde_json::from_value(tree.clone()).unwrap();
        assert_eq!(reading.bound, Some(ResourceBound::Lower));
        assert_eq!(serde_json::to_value(reading).unwrap()["bound"], "lower");
        let group = json!({"name":"compiler","count":1,"cpu_ms":2,"memory_bytes":3,"bound":"lower","future":true});
        let reading: ProcessGroup = serde_json::from_value(group).unwrap();
        assert_eq!(reading.bound, Some(ResourceBound::Lower));
        assert_eq!(serde_json::to_value(reading).unwrap()["bound"], "lower");
        let agents = json!({"module_id":"compiler","cpu_ms":1,"memory_bytes":2,"process_count":0,"bound":"lower","unmeasured_finished":4,"future":true});
        let reading: AgentCommandResources = serde_json::from_value(agents).unwrap();
        assert_eq!(reading.unmeasured_finished, Some(4));
        assert_eq!(serde_json::to_value(reading).unwrap()["bound"], "lower");
    }
}
