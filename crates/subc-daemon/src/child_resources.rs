//! A supervised child's memory and CPU time, read when `supervisor.list` is
//! answered. Report only: nothing here is sampled on a timer, kept, or acted
//! on. The OS reads live in `subc-os`, the one crate allowed the unsafe calls
//! macOS needs.

use subc_control::{
    ChildMemoryKind, ChildResourceReading, ChildResourceUnavailableReason, ChildResourceUsage,
};

/// The resources of the process the supervisor spawned as `pid`.
///
/// `expected_start_time` is the kernel start time recorded at spawn, where the
/// platform provides one (Linux). When it is known, the pid is re-checked
/// after the read, so a reading taken after the child exited and its pid was
/// handed to another process is refused rather than reported as the module's.
/// Without it (macOS) the reading relies on the pid still naming the child,
/// which holds until the supervisor reaps it and the pid space wraps.
pub(crate) fn read(pid: Option<u32>, expected_start_time: Option<u64>) -> ChildResourceUsage {
    read_with(pid, expected_start_time, subc_os::resource_usage, |pid| {
        crate::provenance::process_start_time(pid)
    })
}

fn read_with(
    pid: Option<u32>,
    expected_start_time: Option<u64>,
    resource_usage: impl FnOnce(u32) -> Option<subc_os::ResourceUsage>,
    start_time: impl FnOnce(u32) -> Option<u64>,
) -> ChildResourceUsage {
    let unavailable = |reason| ChildResourceUsage::Unavailable { reason };
    let Some(pid) = pid else {
        return unavailable(ChildResourceUnavailableReason::NotRunning);
    };
    if !subc_os::RESOURCE_USAGE_SUPPORTED {
        return unavailable(ChildResourceUnavailableReason::UnsupportedPlatform);
    }
    let Some(usage) = resource_usage(pid) else {
        return unavailable(ChildResourceUnavailableReason::Unreadable);
    };
    if let Some(expected) = expected_start_time {
        if start_time(pid) != Some(expected) {
            return unavailable(ChildResourceUnavailableReason::ProcessIdentityUnconfirmed);
        }
    }
    ChildResourceUsage::Measured(reading(usage))
}

fn reading(usage: subc_os::ResourceUsage) -> ChildResourceReading {
    let millis =
        |duration: std::time::Duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    ChildResourceReading {
        memory_bytes: usage.memory_bytes,
        memory_kind: match usage.memory_kind {
            subc_os::MemoryKind::PhysFootprint => ChildMemoryKind::PhysFootprint,
            subc_os::MemoryKind::ResidentSet => ChildMemoryKind::ResidentSet,
        },
        swap_bytes: usage.swap_bytes,
        cpu_user_ms: millis(usage.cpu_user),
        cpu_system_ms: millis(usage.cpu_system),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn usage() -> subc_os::ResourceUsage {
        use std::time::Duration;

        subc_os::ResourceUsage {
            memory_bytes: 7 * 1024 * 1024,
            memory_kind: subc_os::MemoryKind::ResidentSet,
            swap_bytes: Some(4096),
            cpu_user: Duration::from_millis(1_250),
            cpu_system: Duration::from_micros(310_900),
        }
    }

    #[test]
    fn no_pid_reads_as_not_running() {
        assert_eq!(
            read(None, None),
            ChildResourceUsage::Unavailable {
                reason: ChildResourceUnavailableReason::NotRunning
            }
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_failed_read_is_unreadable_not_zero() {
        assert_eq!(
            read_with(Some(42), None, |_| None, |_| None),
            ChildResourceUsage::Unavailable {
                reason: ChildResourceUnavailableReason::Unreadable
            }
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_pid_now_naming_another_process_is_refused() {
        assert_eq!(
            read_with(Some(42), Some(100), |_| Some(usage()), |_| Some(101)),
            ChildResourceUsage::Unavailable {
                reason: ChildResourceUnavailableReason::ProcessIdentityUnconfirmed
            }
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_confirmed_read_is_reported_in_wire_units() {
        assert_eq!(
            read_with(Some(42), Some(100), |_| Some(usage()), |_| Some(100)),
            ChildResourceUsage::Measured(ChildResourceReading {
                memory_bytes: 7 * 1024 * 1024,
                memory_kind: ChildMemoryKind::ResidentSet,
                swap_bytes: Some(4096),
                cpu_user_ms: 1_250,
                cpu_system_ms: 310,
            })
        );
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn an_unsupported_platform_says_so() {
        assert_eq!(
            read(Some(std::process::id()), None),
            ChildResourceUsage::Unavailable {
                reason: ChildResourceUnavailableReason::UnsupportedPlatform
            }
        );
    }

    /// The daemon's own process stands in for a child: the read goes through
    /// the real platform source, not a stub.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_live_process_is_measured() {
        let pid = std::process::id();
        let start_time = crate::provenance::process_start_time(pid);
        match read(Some(pid), start_time) {
            ChildResourceUsage::Measured(reading) => assert!(reading.memory_bytes > 0),
            other => panic!("own process was not measured: {other:?}"),
        }
    }
}
