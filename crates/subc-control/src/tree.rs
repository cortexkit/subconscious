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
/// Construct with [`ProcessGroup::new`] and its builders, not a struct literal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProcessGroup {
    pub name: String,
    pub count: u64,
    pub cpu_ms: u64,
    pub memory_bytes: u64,
    /// A live-only attributed CPU floor; absent means exact. Memory is not a floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound: Option<ResourceBound>,
}

/// The module root's attributed share, funded from the module row like groups.
/// Root plus emitted child-group CPU and memory shares never exceed their
/// respective module-row totals.
///
/// Construct this extensible wire type with [`TreeOwn::new`], not a literal.
///
/// ```compile_fail,E0639
/// use subc_control::TreeOwn;
/// let own = TreeOwn { cpu_ms: 1, memory_bytes: 2, bound: None };
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct TreeOwn {
    pub cpu_ms: u64,
    pub memory_bytes: u64,
    /// Absent means exact. This bounds CPU only, not current memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound: Option<ResourceBound>,
}

/// Cumulative CPU and current memory of a module and its descendants.
/// Construct with [`TreeReading::new`] and its builders, not a struct literal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct TreeReading {
    pub cpu_ms: u64,
    pub memory_bytes: u64,
    pub memory_kind: crate::ChildMemoryKind,
    pub process_count: u64,
    pub source: TreeSource,
    /// Absent means exact. Memory and process count are not CPU bounds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound: Option<ResourceBound>,
    /// The root process's attributed share, omitted when unknown. Its CPU is
    /// funded from initial measured CPU and later progress, not its raw counter.
    /// Its bound follows the row's CPU bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own: Option<TreeOwn>,
    /// Up to eight child-only program groups by memory, excluding the root
    /// identity but retaining children with the same name. Absent means none.
    /// Unreadable members invalidate the whole reading, not an empty collection.
    #[serde(default)]
    pub groups: Vec<ProcessGroup>,
}

/// CPU used between the retained wall-clock samples, not a percentage.
/// Construct with [`CpuLastHour::new`] and its builders, not a struct literal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct CpuLastHour {
    pub cpu_ms: u64,
    pub covered_ms: u64,
    /// Only the covered hour is a floor; cumulative CPU has a separate bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound: Option<ResourceBound>,
}

/// The daemon's own process, excluding CPU of its reaped module children.
/// Construct with [`DaemonResources::new`] and its builders, not a struct literal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[non_exhaustive]
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
/// Construct with [`AgentCommandResources::new`] and its builders, not a struct literal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
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

impl ProcessGroup {
    /// Create a measured program group with no CPU lower-bound marker.
    #[must_use]
    pub fn new(name: impl Into<String>, count: u64, cpu_ms: u64, memory_bytes: u64) -> Self {
        Self {
            name: name.into(),
            count,
            cpu_ms,
            memory_bytes,
            bound: None,
        }
    }

    /// Set or clear the CPU lower-bound marker.
    #[must_use]
    pub fn with_bound(mut self, bound: Option<ResourceBound>) -> Self {
        self.bound = bound;
        self
    }
}

impl TreeOwn {
    /// Create the root's attributed share with no CPU lower-bound marker.
    #[must_use]
    pub fn new(cpu_ms: u64, memory_bytes: u64) -> Self {
        Self {
            cpu_ms,
            memory_bytes,
            bound: None,
        }
    }

    /// Set or clear the CPU lower-bound marker.
    #[must_use]
    pub fn with_bound(mut self, bound: Option<ResourceBound>) -> Self {
        self.bound = bound;
        self
    }
}

impl TreeReading {
    /// Create a measured tree without optional root, group or bound metadata.
    #[must_use]
    pub fn new(
        cpu_ms: u64,
        memory_bytes: u64,
        memory_kind: crate::ChildMemoryKind,
        process_count: u64,
        source: TreeSource,
    ) -> Self {
        Self {
            cpu_ms,
            memory_bytes,
            memory_kind,
            process_count,
            source,
            bound: None,
            own: None,
            groups: Vec::new(),
        }
    }

    /// Set or clear the cumulative CPU lower-bound marker.
    #[must_use]
    pub fn with_bound(mut self, bound: Option<ResourceBound>) -> Self {
        self.bound = bound;
        self
    }

    /// Set or clear the root's attributed resource share.
    #[must_use]
    pub fn with_own(mut self, own: Option<TreeOwn>) -> Self {
        self.own = own;
        self
    }

    /// Replace the live child-only program groups; an empty list clears them.
    #[must_use]
    pub fn with_groups(mut self, groups: Vec<ProcessGroup>) -> Self {
        self.groups = groups;
        self
    }
}

impl CpuLastHour {
    /// Create an hourly sample total without a CPU lower-bound marker.
    #[must_use]
    pub fn new(cpu_ms: u64, covered_ms: u64) -> Self {
        Self {
            cpu_ms,
            covered_ms,
            bound: None,
        }
    }

    /// Set or clear the covered hour's CPU lower-bound marker.
    #[must_use]
    pub fn with_bound(mut self, bound: Option<ResourceBound>) -> Self {
        self.bound = bound;
        self
    }
}

impl DaemonResources {
    /// Identify the daemon without manufacturing resource or hourly readings.
    #[must_use]
    pub fn new(pid: u32, start_time_ms: u64, version: impl Into<String>) -> Self {
        Self {
            pid,
            start_time_ms,
            version: version.into(),
            resources: None,
            cpu_last_hour: None,
        }
    }

    /// Set or clear the daemon process's measured usage.
    #[must_use]
    pub fn with_resources(mut self, resources: Option<crate::ChildResourceUsage>) -> Self {
        self.resources = resources;
        self
    }

    /// Set or clear its retained hourly CPU reading.
    #[must_use]
    pub fn with_cpu_last_hour(mut self, cpu_last_hour: Option<CpuLastHour>) -> Self {
        self.cpu_last_hour = cpu_last_hour;
        self
    }
}

impl AgentCommandResources {
    /// Create measured command totals without optional attribution metadata.
    #[must_use]
    pub fn new(
        module_id: impl Into<String>,
        cpu_ms: u64,
        memory_bytes: u64,
        process_count: u64,
    ) -> Self {
        Self {
            module_id: module_id.into(),
            cpu_ms,
            memory_bytes,
            process_count,
            bound: None,
            unmeasured_finished: None,
            groups: Vec::new(),
            cpu_last_hour: None,
        }
    }

    /// Set or clear the cumulative CPU lower-bound marker.
    #[must_use]
    pub fn with_bound(mut self, bound: Option<ResourceBound>) -> Self {
        self.bound = bound;
        self
    }

    /// Set or clear the count of finished commands whose CPU was unmeasured.
    #[must_use]
    pub fn with_unmeasured_finished(mut self, unmeasured_finished: Option<u64>) -> Self {
        self.unmeasured_finished = unmeasured_finished;
        self
    }

    /// Replace live command program groups; an empty list clears them.
    #[must_use]
    pub fn with_groups(mut self, groups: Vec<ProcessGroup>) -> Self {
        self.groups = groups;
        self
    }

    /// Set or clear retained hourly CPU attributed to commands.
    #[must_use]
    pub fn with_cpu_last_hour(mut self, cpu_last_hour: Option<CpuLastHour>) -> Self {
        self.cpu_last_hour = cpu_last_hour;
        self
    }
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
        // The previous tree reader has no root-share field and accepts unknown
        // fields. A new daemon must remain decodable by that older shape.
        #[derive(Deserialize, Serialize)]
        struct OlderTreeReading {
            cpu_ms: u64,
            memory_bytes: u64,
            memory_kind: crate::ChildMemoryKind,
            process_count: u64,
            source: TreeSource,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            bound: Option<ResourceBound>,
            #[serde(default)]
            groups: Vec<ProcessGroup>,
        }
        let reading = TreeReading {
            bound: None,
            own: Some(TreeOwn {
                cpu_ms: 23,
                memory_bytes: 56,
                bound: None,
            }),
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
        value["modules"][0]["tree"]["own"]["future"] = serde_json::json!(true);
        value["modules"][0]["tree"]["groups"][0]["future"] = serde_json::json!(3);
        value["daemon"]["future"] = serde_json::json!(4);
        value["agent_commands"][0]["future"] = serde_json::json!(5);
        let older: OlderTreeReading =
            serde_json::from_value(value["modules"][0]["tree"].clone()).unwrap();
        let older = serde_json::to_value(older).unwrap();
        assert!(older.get("own").is_none());
        assert_eq!(older["cpu_ms"], 123);
        assert_eq!(older["groups"][0]["cpu_ms"], 100);
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
        assert!(reading.own.is_none());
        assert!(serde_json::to_value(&reading).unwrap().get("own").is_none());
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
        let own = json!({"cpu_ms":2,"memory_bytes":3,"future":true});
        let reading: TreeOwn = serde_json::from_value(own.clone()).unwrap();
        assert!(reading.bound.is_none());
        assert_eq!(
            serde_json::to_value(reading).unwrap(),
            json!({"cpu_ms":2,"memory_bytes":3})
        );
        refuses_missing::<TreeOwn>(&own, &["cpu_ms", "memory_bytes"]);
    }

    #[test]
    fn lower_bounds_and_unmeasured_finishes_round_trip_additively() {
        let tree = json!({"cpu_ms":1,"memory_bytes":2,"memory_kind":"resident_set","process_count":3,"source":"process_tree","bound":"lower","own":{"cpu_ms":1,"memory_bytes":2,"bound":"lower","future":true},"future":true});
        let reading: TreeReading = serde_json::from_value(tree.clone()).unwrap();
        assert_eq!(reading.bound, Some(ResourceBound::Lower));
        assert_eq!(
            reading.own.as_ref().unwrap().bound,
            Some(ResourceBound::Lower)
        );
        let encoded = serde_json::to_value(reading).unwrap();
        assert_eq!(encoded["bound"], "lower");
        assert_eq!(
            encoded["own"],
            json!({"cpu_ms":1,"memory_bytes":2,"bound":"lower"})
        );
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
