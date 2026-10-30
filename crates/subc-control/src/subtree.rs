use serde::{Deserialize, Serialize};

open_string_enum! {
    /// The measurement behind a module's memory including child processes.
    #[non_exhaustive]
    SubtreeSource {
        CgroupMemory => "cgroup_memory",
        ProcessSum => "process_sum",
        Unavailable => "unavailable"
    }
}

/// A report-only memory reading of a module's process container.
///
/// Use the constructors rather than struct literals so new fields stay additive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct SubtreeReading {
    pub source: SubtreeSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// Absent means swap is not accounted, not zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl SubtreeReading {
    /// Cgroup charges, including page cache, rather than summed resident sets.
    pub fn cgroup_memory(memory_bytes: u64, swap_bytes: Option<u64>, process_count: u64) -> Self {
        Self {
            source: SubtreeSource::CgroupMemory,
            memory_bytes: Some(memory_bytes),
            swap_bytes,
            process_count: Some(process_count),
            reason: None,
        }
    }

    /// Sum of process resident sets. Shared pages count once per process.
    pub fn process_sum(memory_bytes: u64, swap_bytes: Option<u64>, process_count: u64) -> Self {
        Self {
            source: SubtreeSource::ProcessSum,
            memory_bytes: Some(memory_bytes),
            swap_bytes,
            process_count: Some(process_count),
            reason: None,
        }
    }

    /// No container-wide figure could be obtained; never substitutes a PID reading.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            source: SubtreeSource::Unavailable,
            memory_bytes: None,
            swap_bytes: None,
            process_count: None,
            reason: Some(reason.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{SubtreeReading, SupervisorEntry, SupervisorHealthStatus};

    #[test]
    fn absent_subtree_preserves_exact_list_entry_bytes() {
        let entry =
            SupervisorEntry::new("m", "running", true, true, SupervisorHealthStatus::Unknown);
        assert_eq!(
            serde_json::to_string(&entry).unwrap(),
            r#"{"module_id":"m","state":"running","enabled":true,"live":true,"protocol":"subc","health":"unknown","last_probe_ms":null}"#
        );
    }

    #[test]
    fn cgroup_swap_absence_is_omitted_not_zero() {
        let reading = SubtreeReading::cgroup_memory(8192, None, 3);
        assert_eq!(
            serde_json::to_value(reading).unwrap(),
            serde_json::json!({
                "source": "cgroup_memory", "memory_bytes": 8192, "process_count": 3
            })
        );
    }

    #[test]
    fn older_entry_decoder_ignores_subtree() {
        #[derive(serde::Deserialize)]
        struct LegacyEntry {
            module_id: String,
        }
        let entry =
            SupervisorEntry::new("m", "running", true, true, SupervisorHealthStatus::Unknown)
                .with_subtree(Some(SubtreeReading::process_sum(4096, None, 3)));
        let bytes = serde_json::to_vec(&entry).unwrap();
        let old: LegacyEntry = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(old.module_id, "m");
        assert!(!String::from_utf8(bytes).unwrap().contains("swap_bytes"));
    }
}
