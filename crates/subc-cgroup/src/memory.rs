//! Report-only cgroup subtree memory. No controller settings are changed.

use std::{collections::BTreeSet, fs, io, path::Path, time::Instant};

/// The two Linux memory measurements have different accounting semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MemorySource {
    /// Kernel charges, including page cache.
    CgroupMemory,
    /// Resident sets summed per process, counting shared pages per process.
    ProcessSum,
}

/// One report-only reading; use [`read_memory`] to construct it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MemoryReading {
    pub source: MemorySource,
    pub memory_bytes: u64,
    /// Absent means swap is not accounted.
    pub swap_bytes: Option<u64>,
    pub process_count: u64,
}

/// A failed reading retains the process count when enumeration completed.
#[derive(Debug)]
#[non_exhaustive]
pub struct MemoryReadError {
    pub error: io::Error,
    pub process_count: Option<u64>,
}

impl MemoryReadError {
    pub fn kind(&self) -> io::ErrorKind {
        self.error.kind()
    }
}

impl From<io::Error> for MemoryReadError {
    fn from(error: io::Error) -> Self {
        Self {
            error,
            process_count: None,
        }
    }
}

impl std::fmt::Display for MemoryReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for MemoryReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

fn check_deadline(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "memory read deadline",
        ))
    } else {
        Ok(())
    }
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid memory reading")
}

/// Read a module cgroup and all child cgroups, with procfs supplied explicitly.
///
/// The deadline stops traversal between filesystem calls. Callers must also
/// bound their wait off-thread, because a filesystem call itself can stall.
pub fn read_memory(
    root: &Path,
    proc_root: &Path,
    deadline: Instant,
) -> Result<MemoryReading, MemoryReadError> {
    read_memory_counted(root, proc_root, deadline, |_| {})
}

/// Like [`read_memory`], publishing the count before reading process memory.
/// A caller that bounds its wait can retain that count even if procfs stalls.
pub fn read_memory_counted(
    root: &Path,
    proc_root: &Path,
    deadline: Instant,
    mut counted: impl FnMut(u64),
) -> Result<MemoryReading, MemoryReadError> {
    check_deadline(deadline)?;
    let current = match fs::read_to_string(root.join("memory.current")) {
        Ok(text) => Some(text.trim().parse::<u64>().map_err(|_| invalid())?),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(err.into()),
    };
    let mut pids = BTreeSet::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(group) = pending.pop() {
        check_deadline(deadline)?;
        for pid in fs::read_to_string(group.join("cgroup.procs"))?.split_whitespace() {
            check_deadline(deadline)?;
            pids.insert(pid.parse::<u32>().map_err(|_| invalid())?);
        }
        for entry in fs::read_dir(group)? {
            check_deadline(deadline)?;
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    let mut process_count = pids.len() as u64;
    counted(process_count);
    let result = (|| -> io::Result<MemoryReading> {
        let reading = if let Some(memory_bytes) = current {
            let swap_bytes = match fs::read_to_string(root.join("memory.swap.current")) {
                Ok(text) => Some(text.trim().parse::<u64>().map_err(|_| invalid())?),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            MemoryReading {
                source: MemorySource::CgroupMemory,
                memory_bytes,
                swap_bytes,
                process_count,
            }
        } else {
            let mut memory_bytes = 0_u64;
            let mut swap_bytes = 0_u64;
            for pid in pids {
                check_deadline(deadline)?;
                let status =
                    match fs::read_to_string(proc_root.join(pid.to_string()).join("status")) {
                        Ok(status) => status,
                        Err(error) if process_exited(&error) => {
                            process_count -= 1;
                            counted(process_count);
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                let rss = status_kib(&status, "VmRSS:")?;
                let swap = status_kib(&status, "VmSwap:")?;
                memory_bytes = memory_bytes.checked_add(rss).ok_or_else(invalid)?;
                swap_bytes = swap_bytes.checked_add(swap).ok_or_else(invalid)?;
            }
            MemoryReading {
                source: MemorySource::ProcessSum,
                memory_bytes,
                swap_bytes: Some(swap_bytes),
                process_count,
            }
        };
        check_deadline(deadline)?;
        Ok(reading)
    })();
    result.map_err(|error| MemoryReadError {
        error,
        process_count: Some(process_count),
    })
}

fn status_kib(status: &str, key: &str) -> io::Result<u64> {
    let Some(line) = status.lines().find_map(|line| line.strip_prefix(key)) else {
        // Zombies have no memory counters but remain members until reaped.
        return Ok(0);
    };
    let mut words = line.split_whitespace();
    let value = words
        .next()
        .ok_or_else(invalid)?
        .parse::<u64>()
        .map_err(|_| invalid())?;
    if words.next() != Some("kB") || words.next().is_some() {
        return Err(invalid());
    }
    value.checked_mul(1024).ok_or_else(invalid)
}

fn process_exited(error: &io::Error) -> bool {
    // ESRCH is the no-such-process error.
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;
    use std::time::Duration;

    struct Tree(ScratchDir);

    impl Tree {
        fn new() -> Self {
            let root = ScratchDir::new("cgroup-memory");
            fs::create_dir_all(root.join("group")).unwrap();
            fs::create_dir_all(root.join("proc")).unwrap();
            fs::write(root.join("group/cgroup.procs"), "11\n").unwrap();
            Self(root)
        }

        fn write(&self, path: &str, value: &str) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, value).unwrap();
        }

        fn read(&self) -> Result<MemoryReading, MemoryReadError> {
            read_memory(
                &self.0.join("group"),
                &self.0.join("proc"),
                Instant::now() + Duration::from_secs(1),
            )
        }
    }

    #[test]
    fn zombie_status_without_memory_fields_still_counts() {
        let tree = Tree::new();
        tree.write("proc/11/status", "Name:\tzombie\nState:\tZ (zombie)\n");
        let reading = tree.read().unwrap();
        assert_eq!(reading.source, MemorySource::ProcessSum);
        assert_eq!(reading.memory_bytes, 0);
        assert_eq!(reading.swap_bytes, Some(0));
        assert_eq!(reading.process_count, 1);
    }

    #[test]
    fn process_exiting_after_enumeration_is_not_counted() {
        let tree = Tree::new();
        tree.write("group/cgroup.procs", "11\n12\n");
        tree.write("proc/11/status", "VmRSS:\t32 kB\nVmSwap:\t2 kB\n");
        let reading = tree.read().unwrap();
        assert_eq!(reading.memory_bytes, 32768);
        assert_eq!(reading.swap_bytes, Some(2048));
        assert_eq!(reading.process_count, 1);
    }

    #[test]
    fn cgroup_without_swap_accounting_keeps_memory_reading() {
        let tree = Tree::new();
        tree.write("group/memory.current", "8192\n");
        let reading = tree.read().unwrap();
        assert_eq!(reading.source, MemorySource::CgroupMemory);
        assert_eq!(reading.memory_bytes, 8192);
        assert_eq!(reading.swap_bytes, None);
        assert_eq!(reading.process_count, 1);
    }

    #[test]
    fn cgroup_charges_include_nested_process_count_without_proc_reads() {
        let tree = Tree::new();
        tree.write("group/memory.current", "8192\n");
        tree.write("group/memory.swap.current", "4096\n");
        tree.write("group/nested/cgroup.procs", "12\n13\n");
        let reading = tree.read().unwrap();
        assert_eq!(reading.source, MemorySource::CgroupMemory);
        assert_eq!(
            (
                reading.memory_bytes,
                reading.swap_bytes,
                reading.process_count
            ),
            (8192, Some(4096), 3)
        );
    }

    #[test]
    fn process_sum_covers_child_cgroups_and_deduplicates_pids() {
        let tree = Tree::new();
        tree.write("group/nested/cgroup.procs", "12\n11\n");
        tree.write("group/nested/deeper/cgroup.procs", "13\n");
        tree.write(
            "proc/11/status",
            "Name:\tmodule\nVmRSS:\t10 kB\nVmSwap:\t3 kB\n",
        );
        tree.write("proc/12/status", "VmRSS:\t20 kB\nVmSwap:\t0 kB\n");
        tree.write("proc/13/status", "VmRSS:\t40 kB\nVmSwap:\t7 kB\n");
        let reading = tree.read().unwrap();
        assert_eq!(reading.source, MemorySource::ProcessSum);
        assert_eq!(
            (
                reading.memory_bytes,
                reading.swap_bytes,
                reading.process_count
            ),
            (71680, Some(10240), 3)
        );
    }

    #[test]
    fn missing_cgroup_is_unavailable_not_zero() {
        let tree = Tree::new();
        fs::remove_file(tree.0.join("group/cgroup.procs")).unwrap();
        assert_eq!(tree.read().unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn disappeared_process_errors_are_distinct_from_unreadable_processes() {
        assert!(process_exited(&io::Error::from(io::ErrorKind::NotFound)));
        assert!(process_exited(&io::Error::from_raw_os_error(3)));
        assert!(!process_exited(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
    }

    #[test]
    fn absent_fields_are_zero_but_present_malformed_fields_are_errors() {
        assert_eq!(status_kib("VmSwap:\t2 kB\n", "VmRSS:").unwrap(), 0);
        assert_eq!(status_kib("VmRSS:\t2 kB\n", "VmSwap:").unwrap(), 0);
        for status in ["VmRSS:\n", "VmRSS:\tbad kB\n", "VmRSS:\t1 MB\n"] {
            assert_eq!(
                status_kib(status, "VmRSS:").unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn malformed_controller_or_proc_data_is_not_a_partial_sum() {
        let tree = Tree::new();
        tree.write("group/memory.current", "bad\n");
        assert_eq!(tree.read().unwrap_err().kind(), io::ErrorKind::InvalidData);
        fs::remove_file(tree.0.join("group/memory.current")).unwrap();
        tree.write("proc/11/status", "VmRSS:\t10 MB\nVmSwap:\t0 kB\n");
        assert_eq!(tree.read().unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn expired_deadline_refuses_to_read_even_an_existing_tree() {
        let tree = Tree::new();
        let err =
            read_memory(&tree.0.join("group"), &tree.0.join("proc"), Instant::now()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }
}
