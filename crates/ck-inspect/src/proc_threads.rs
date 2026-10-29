use std::{collections::BTreeMap, fs, path::Path};

use crate::profile::{Profile, Thread};

#[derive(Debug)]
pub(crate) struct Reading {
    ticks: u64,
    started: u64,
    name: String,
}

pub(crate) fn read_tasks(root: &Path) -> Result<BTreeMap<u32, Reading>, &'static str> {
    let mut readings = BTreeMap::new();
    for entry in fs::read_dir(root).map_err(|_| "cannot read process tasks")? {
        let Ok(entry) = entry else { continue };
        let Some(tid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        // Threads can exit between directory enumeration and either file read.
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Ok(name) = fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        let Some((_, fields)) = stat.rsplit_once(')') else {
            continue;
        };
        let fields: Vec<_> = fields.split_whitespace().collect();
        let number = |index| fields.get(index).and_then(|s: &&str| s.parse::<u64>().ok());
        let (Some(user), Some(system), Some(started)) = (number(11), number(12), number(19)) else {
            continue;
        };
        readings.insert(
            tid,
            Reading {
                ticks: user.saturating_add(system),
                started,
                name: name.trim_end().into(),
            },
        );
    }
    if readings.is_empty() {
        return Err("process has no readable tasks");
    }
    Ok(readings)
}

pub(crate) fn summarize(
    start: BTreeMap<u32, Reading>,
    end: BTreeMap<u32, Reading>,
    hz: u64,
    mut profile: Profile,
) -> Profile {
    for (tid, after) in end {
        let Some(before) = start.get(&tid).filter(|b| b.started == after.started) else {
            continue;
        };
        let delta = after.ticks.saturating_sub(before.ticks);
        if delta == 0 {
            profile.idle_threads += 1;
            continue;
        }
        let mut thread = Thread::new(after.name);
        thread.cpu_ms = Some(delta.saturating_mul(1000) / hz.max(1));
        profile.threads.push(thread);
    }
    profile
        .threads
        .sort_by(|a, b| b.cpu_ms.cmp(&a.cpu_ms).then(a.name.cmp(&b.name)));
    profile.truncated = profile.threads.len() > 8;
    profile.threads.truncate(8);
    profile
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn proc_threads_keep_the_eight_highest_cpu_deltas() {
        let mut start = BTreeMap::new();
        let mut end = BTreeMap::new();
        for i in 1..=12 {
            start.insert(
                i,
                Reading {
                    ticks: 0,
                    started: 1,
                    name: format!("thread-{i}"),
                },
            );
            end.insert(
                i,
                Reading {
                    ticks: i.into(),
                    started: 1,
                    name: format!("thread-{i}"),
                },
            );
        }
        let result = summarize(
            start,
            end,
            100,
            Profile::new("fixture".into(), 1, 1, "proc_threads".into()),
        );
        assert_eq!(result.threads.len(), 8);
        assert_eq!(result.threads[0].cpu_ms, Some(120));
        assert_eq!(result.threads[7].cpu_ms, Some(50));
        assert!(result.truncated);
    }
    #[test]
    fn proc_fixture_handles_spaces_ticks_idle_and_reused_thread_ids() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/proc");
        let result = summarize(
            read_tasks(&root.join("start")).unwrap(),
            read_tasks(&root.join("end")).unwrap(),
            100,
            Profile::new("fixture".into(), 9, 1, "proc_threads".into()),
        );
        assert_eq!(result.idle_threads, 1);
        assert_eq!(result.threads.len(), 2);
        assert_eq!(result.threads[0].name, "hot worker");
        assert_eq!(result.threads[0].cpu_ms, Some(250));
        assert_eq!(result.threads[1].cpu_ms, Some(10));
        assert!(result.threads[0].stacks.is_empty());
        assert!(read_tasks(&root.join("absent")).is_err());
    }
}
