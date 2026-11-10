//! Read-only cgroup-v2 CPU and membership accounting.

use std::collections::BTreeSet;
use std::io;
use std::path::Path;
use std::time::Instant;

/// Cumulative CPU of this cgroup and its descendants, including exited tasks.
/// The kernel reports microseconds; the result is truncated to milliseconds.
pub fn read_cpu_ms(root: &Path) -> io::Result<u64> {
    let text = std::fs::read_to_string(root.join("cpu.stat"))?;
    text.lines()
        .find_map(|line| {
            let (key, value) = line.split_once(' ')?;
            (key == "usage_usec").then(|| value.trim().parse::<u64>())
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "cpu.stat has no usage_usec"))?
        .map(|usage| usage / 1_000)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Deduplicated PIDs in descendant cgroups, without freezing or writing them.
pub fn process_ids(root: &Path, deadline: Instant) -> io::Result<Vec<u32>> {
    let mut pids = BTreeSet::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(group) = pending.pop() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "cgroup membership deadline",
            ));
        }
        for pid in std::fs::read_to_string(group.join("cgroup.procs"))?.split_whitespace() {
            let pid = pid
                .parse::<u32>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if pid > 0 {
                pids.insert(pid);
            }
        }
        for entry in std::fs::read_dir(group)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(pids.into_iter().collect())
}
