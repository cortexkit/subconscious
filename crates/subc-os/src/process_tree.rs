//! Read-only process-table snapshots. No process is stopped or signalled.

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use crate::{MemoryKind, ResourceUsage};

/// A process identity and its observed parent, session, program and resources.
#[derive(Debug, Clone)]
pub struct ProcessMember {
    pub pid: u32,
    pub parent_pid: u32,
    pub session_id: Option<u32>,
    /// Native kernel units, suitable for exact PID-reuse checks.
    pub start_time: u64,
    pub start_time_ms: u64,
    pub name: String,
    /// Unix CPU includes reaped children; Windows CPU is the process's own.
    pub resources: Option<ResourceUsage>,
    /// Separate own and reaped-child counters from the same native observation.
    pub own_cpu_ms: Option<u64>,
    pub reaped_cpu_ms: Option<u64>,
    /// Query handle pinned to this Windows identity, usable after its exit.
    #[cfg(windows)]
    pub process: std::sync::Arc<crate::Process>,
}

/// A live program group. CPU includes its members' reaped children on Unix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessGroup {
    pub name: String,
    pub count: u64,
    pub cpu_ms: u64,
    pub memory_bytes: u64,
}

/// Cumulative CPU and current memory of the observed members of a tree.
#[derive(Debug, Clone)]
pub struct TreeUsage {
    pub cpu_ms: u64,
    pub memory_bytes: u64,
    pub memory_kind: MemoryKind,
    pub process_count: u64,
    pub groups: Vec<ProcessGroup>,
    pub members: Vec<ProcessMember>,
}

/// One process-table walk, reusable for any number of roots.
#[derive(Debug, Clone, Default)]
pub struct ProcessTable {
    members: BTreeMap<u32, ProcessMember>,
    unreadable: BTreeMap<u32, (Option<u32>, Option<u32>)>,
    revision: u64,
    observed_at_ms: u64,
    started_at: Option<std::time::Instant>,
}

impl ProcessTable {
    /// Inject a complete or explicitly unreadable snapshot for downstream
    /// accounting tests. This constructor is absent from normal library builds.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn from_test_members(
        revision: u64,
        observed_at_ms: u64,
        members: Vec<ProcessMember>,
        unreadable: Vec<(u32, Option<u32>, Option<u32>)>,
    ) -> Self {
        Self {
            members: members
                .into_iter()
                .map(|member| (member.pid, member))
                .collect(),
            unreadable: unreadable
                .into_iter()
                .map(|(pid, parent, session)| (pid, (parent, session)))
                .collect(),
            revision,
            observed_at_ms,
            started_at: Some(std::time::Instant::now()),
        }
    }
    /// Read all visible processes once. Unreadable identities are retained as
    /// uncertainty, so a related tree cannot silently become a partial sum.
    pub fn read() -> io::Result<Self> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let revision = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let observed_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
            .unwrap_or(0);
        let started_at = std::time::Instant::now();
        #[cfg(windows)]
        let snapshot_started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos()
            / 100
            + 116_444_736_000_000_000;
        let mut members = BTreeMap::new();
        let mut unreadable = BTreeMap::new();
        for (pid, parent, name, expected_start) in entries()? {
            let parent = parent.or_else(|| crate::parent_pid(pid));
            #[cfg(unix)]
            let session_id = {
                // SAFETY: getsid is a read-only query of a numeric process.
                #[allow(unsafe_code)]
                let session = unsafe { libc::getsid(pid as libc::pid_t) };
                u32::try_from(session).ok()
            };
            #[cfg(not(unix))]
            let session_id = None;
            #[cfg(windows)]
            let Some(process) = crate::Process::open(pid).ok().flatten() else {
                unreadable.insert(pid, (parent, session_id));
                continue;
            };
            #[cfg(windows)]
            let start = process.observe().map(|observation| observation.start_time);
            #[cfg(not(windows))]
            let start = match crate::read_identity(pid) {
                crate::IdentityRead::Alive(start) => Some(start),
                crate::IdentityRead::Gone => continue,
                crate::IdentityRead::Unknown => None,
            };
            let Some(start_time) =
                start.filter(|start| expected_start.is_none_or(|expected| expected == *start))
            else {
                if possibly_live(pid) {
                    unreadable.insert(pid, (parent, session_id));
                }
                continue;
            };
            #[cfg(windows)]
            if u128::from(start_time) > snapshot_started {
                continue;
            }
            let Some(start_time_ms) = crate::start_time_ms_from_native(start_time) else {
                unreadable.insert(pid, (parent, session_id));
                continue;
            };
            let parent_pid = match parent {
                Some(parent) => parent,
                None => {
                    unreadable.insert(pid, (None, session_id));
                    continue;
                }
            };
            #[cfg(target_os = "linux")]
            let parts = crate::linux::resource_usage_parts(pid);
            #[cfg(target_os = "macos")]
            let parts = crate::macos::resource_usage_parts(pid);
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let (resources, own_cpu_ms, reaped_cpu_ms) = parts.map_or(
                (None, None, None),
                |(mut usage, child_user, child_system)| {
                    let millis = |duration: std::time::Duration| {
                        u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
                    };
                    let own = millis(usage.cpu_user + usage.cpu_system);
                    let reaped = millis(child_user + child_system);
                    usage.cpu_user += child_user;
                    usage.cpu_system += child_system;
                    (Some(usage), Some(own), Some(reaped))
                },
            );
            #[cfg(windows)]
            let resources = process.resource_usage();
            #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
            let resources = crate::resource_usage(pid);
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            let own_cpu_ms = resources.map(|usage| {
                u64::try_from((usage.cpu_user + usage.cpu_system).as_millis()).unwrap_or(u64::MAX)
            });
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            let reaped_cpu_ms = resources.map(|_| 0);
            #[cfg(windows)]
            let current_start = process
                .observe()
                .map_or(crate::IdentityRead::Unknown, |observation| {
                    crate::IdentityRead::Alive(observation.start_time)
                });
            #[cfg(not(windows))]
            let current_start = crate::read_identity(pid);
            if !Self::same_identity(
                pid,
                start_time,
                (Some(parent_pid), session_id),
                current_start,
                &mut unreadable,
            ) {
                continue;
            }
            members.insert(
                pid,
                ProcessMember {
                    pid,
                    parent_pid,
                    session_id,
                    start_time,
                    start_time_ms,
                    name,
                    resources,
                    own_cpu_ms,
                    reaped_cpu_ms,
                    #[cfg(windows)]
                    process: std::sync::Arc::new(process),
                },
            );
        }
        // A reaped child's earlier sample can overlap a later parent's reaped
        // counter. Drop confirmed exits, leaving recovery to that counter, but
        // reject unknown identities rather than publishing a partial live sum.
        Self::fence(&mut members, &mut unreadable, |member| {
            #[cfg(windows)]
            let current = member
                .process
                .observe()
                .map_or(crate::IdentityRead::Unknown, |observation| {
                    crate::IdentityRead::Alive(observation.start_time)
                });
            #[cfg(not(windows))]
            let current = crate::read_identity(member.pid);
            current
        });
        Ok(Self {
            members,
            unreadable,
            revision,
            observed_at_ms,
            started_at: Some(started_at),
        })
    }

    fn fence(
        members: &mut BTreeMap<u32, ProcessMember>,
        unreadable: &mut BTreeMap<u32, (Option<u32>, Option<u32>)>,
        mut identity: impl FnMut(&ProcessMember) -> crate::IdentityRead,
    ) {
        members.retain(|pid, member| {
            Self::same_identity(
                *pid,
                member.start_time,
                (Some(member.parent_pid), member.session_id),
                identity(member),
                unreadable,
            )
        });
    }

    fn same_identity(
        pid: u32,
        start: u64,
        ownership: (Option<u32>, Option<u32>),
        current: crate::IdentityRead,
        unreadable: &mut BTreeMap<u32, (Option<u32>, Option<u32>)>,
    ) -> bool {
        match current {
            crate::IdentityRead::Alive(now) if now == start => true,
            crate::IdentityRead::Gone => false,
            _ => {
                unreadable.insert(pid, ownership);
                false
            }
        }
    }

    /// Start of the walk, so exit confirmations can distinguish a
    /// genuinely newer snapshot from a slow, earlier walk completing late.
    pub fn started_at(&self) -> Option<std::time::Instant> {
        self.started_at
    }

    pub fn process(&self, pid: u32) -> Option<&ProcessMember> {
        self.members.get(&pid)
    }

    /// A process-local walk sequence, allocated before the walk starts. A slow
    /// old walk cannot overwrite a newer accounting observation that finished first.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Wall-clock boundary before enumeration, for conservative birth checks.
    pub fn observed_at_ms(&self) -> u64 {
        self.observed_at_ms
    }

    /// `None` means the PID was visible but its identity was unreadable, not that
    /// the tracked process has exited. A different readable start identity is gone.
    pub fn identity_alive(&self, pid: u32, start: u64) -> Option<bool> {
        if self.unreadable.contains_key(&pid) {
            return None;
        }
        Some(
            self.process(pid)
                .is_some_and(|member| member.start_time == start),
        )
    }

    /// All identities successfully read in this walk, including resources that
    /// could not be read. Consumers must not interpret those resources as zero.
    pub fn members(&self) -> impl Iterator<Item = &ProcessMember> {
        self.members.values()
    }

    /// Resolve descendants, optionally including a command leader's session.
    /// The root must still have the expected native kernel start identity.
    pub fn tree(&self, pid: u32, start_time: u64, include_session: bool) -> Option<TreeUsage> {
        Self::sum(self.tree_members(pid, start_time, include_session)?)
    }

    /// Structural ownership does not depend on readable resource counters.
    /// Unknown identities still refuse the result, rather than hiding overlap.
    pub fn tree_identities(
        &self,
        pid: u32,
        start_time: u64,
        include_session: bool,
    ) -> Option<BTreeSet<(u32, u64)>> {
        Some(
            self.tree_members(pid, start_time, include_session)?
                .into_iter()
                .map(|member| (member.pid, member.start_time))
                .collect(),
        )
    }

    fn tree_members(
        &self,
        pid: u32,
        start_time: u64,
        include_session: bool,
    ) -> Option<Vec<ProcessMember>> {
        let root = self.members.get(&pid)?;
        if root.start_time != start_time {
            return None;
        }
        let session = include_session
            .then_some(root.session_id)
            .flatten()
            .filter(|id| *id == pid);
        let mut selected = BTreeSet::from([pid]);
        loop {
            let before = selected.len();
            for member in self.members.values() {
                if selected.contains(&member.parent_pid)
                    || session.is_some_and(|id| member.session_id == Some(id))
                {
                    // A reused parent PID must not acquire older processes.
                    if self
                        .members
                        .get(&member.parent_pid)
                        .is_none_or(|parent| parent.start_time <= member.start_time)
                    {
                        selected.insert(member.pid);
                    }
                }
            }
            if selected.len() == before {
                break;
            }
        }
        if self
            .unreadable
            .iter()
            .any(|(pid, (parent, unreadable_session))| {
                selected.contains(pid)
                    || parent.is_none_or(|parent| selected.contains(&parent))
                    || session.is_some_and(|session| {
                        unreadable_session.is_none_or(|unknown| unknown == session)
                    })
            })
        {
            return None;
        }
        let members: Vec<_> = selected
            .into_iter()
            .filter_map(|pid| self.members.get(&pid).cloned())
            .collect();
        Some(members)
    }

    /// Sum distinct members, returning at most eight groups by descending memory.
    pub fn sum(members: Vec<ProcessMember>) -> Option<TreeUsage> {
        let memory_kind = members
            .iter()
            .find_map(|m| m.resources.map(|r| r.memory_kind))?;
        let mut result = TreeUsage {
            cpu_ms: 0,
            memory_bytes: 0,
            memory_kind,
            process_count: 0,
            groups: Vec::new(),
            members,
        };
        let mut groups = BTreeMap::<String, ProcessGroup>::new();
        let mut seen = BTreeSet::new();
        for member in &result.members {
            if !seen.insert((member.pid, member.start_time)) {
                continue;
            }
            let resources = member.resources?;
            let cpu_ms = u64::try_from((resources.cpu_user + resources.cpu_system).as_millis())
                .unwrap_or(u64::MAX);
            result.cpu_ms = result.cpu_ms.saturating_add(cpu_ms);
            result.memory_bytes = result.memory_bytes.saturating_add(resources.memory_bytes);
            result.process_count += 1;
            let group = groups
                .entry(member.name.clone())
                .or_insert_with(|| ProcessGroup {
                    name: member.name.clone(),
                    count: 0,
                    cpu_ms: 0,
                    memory_bytes: 0,
                });
            group.count += 1;
            group.cpu_ms = group.cpu_ms.saturating_add(cpu_ms);
            group.memory_bytes = group.memory_bytes.saturating_add(resources.memory_bytes);
        }
        result.groups = groups.into_values().collect();
        result.groups.sort_by(|a, b| {
            b.memory_bytes
                .cmp(&a.memory_bytes)
                .then_with(|| a.name.cmp(&b.name))
        });
        result.groups.truncate(8);
        Some(result)
    }
}

#[cfg(unix)]
fn possibly_live(pid: u32) -> bool {
    // SAFETY: getsid is a read-only query and does not send a signal.
    #[allow(unsafe_code)]
    let session = unsafe { libc::getsid(pid as libc::pid_t) };
    session >= 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn possibly_live(_pid: u32) -> bool {
    true
}

type Entry = (u32, Option<u32>, String, Option<u64>);

#[cfg(target_os = "linux")]
fn entries() -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            entries.push((pid, None, String::new(), None));
            continue;
        };
        let Some((name, rest)) = stat
            .split_once('(')
            .and_then(|(_, tail)| tail.rsplit_once(')'))
        else {
            entries.push((pid, None, String::new(), None));
            continue;
        };
        let parent = rest.split_whitespace().nth(1).and_then(|s| s.parse().ok());
        let start = rest
            .split_whitespace()
            .nth(19)
            .and_then(|value| value.parse().ok());
        entries.push((pid, parent, name.to_owned(), start));
    }
    Ok(entries)
}

#[cfg(target_os = "macos")]
fn entries() -> io::Result<Vec<Entry>> {
    #[allow(unsafe_code)]
    unsafe extern "C" {
        fn proc_listallpids(buffer: *mut std::ffi::c_void, size: libc::c_int) -> libc::c_int;
        fn proc_name(pid: libc::c_int, buffer: *mut std::ffi::c_void, size: u32) -> libc::c_int;
    }
    // SAFETY: a null buffer asks only for the number of PIDs.
    #[allow(unsafe_code)]
    let count = unsafe { proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Err(io::Error::last_os_error());
    }
    let mut pids = vec![0_i32; count as usize + 256];
    // SAFETY: pids is an initialized writable buffer of the advertised size.
    #[allow(unsafe_code)]
    let count = unsafe {
        proc_listallpids(
            pids.as_mut_ptr().cast(),
            (pids.len() * size_of::<i32>()) as i32,
        )
    };
    if count <= 0 {
        return Err(io::Error::last_os_error());
    }
    if count as usize >= pids.len() {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "process table outgrew its snapshot buffer",
        ));
    }
    let mut entries = Vec::new();
    for pid in pids.into_iter().take(count as usize).filter(|pid| *pid > 0) {
        let mut name = [0_u8; 1024];
        // SAFETY: name is initialized, writable and its length is correct.
        #[allow(unsafe_code)]
        let length = unsafe { proc_name(pid, name.as_mut_ptr().cast(), name.len() as u32) };
        let length = usize::try_from(length).unwrap_or(0).min(name.len());
        let name = String::from_utf8_lossy(&name[..length])
            .trim_end_matches('\0')
            .to_owned();
        entries.push((pid as u32, None, name, None));
    }
    Ok(entries)
}

#[cfg(windows)]
fn entries() -> io::Result<Vec<Entry>> {
    crate::windows::process_entries().map(|entries| {
        entries
            .into_iter()
            .map(|(pid, parent, name)| (pid, parent, name, None))
            .collect()
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn entries() -> io::Result<Vec<Entry>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process tables unavailable",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn member(
        pid: u32,
        parent_pid: u32,
        start_time: u64,
        session_id: u32,
        memory_bytes: u64,
    ) -> ProcessMember {
        ProcessMember {
            pid,
            parent_pid,
            start_time,
            start_time_ms: start_time,
            session_id: Some(session_id),
            name: format!("program-{pid}"),
            resources: Some(ResourceUsage {
                memory_bytes,
                memory_kind: MemoryKind::ResidentSet,
                swap_bytes: None,
                cpu_user: Duration::from_millis(memory_bytes * 10),
                cpu_system: Duration::ZERO,
            }),
            own_cpu_ms: Some(memory_bytes * 10),
            reaped_cpu_ms: Some(0),
            // These synthetic arithmetic fixtures never query the handle.
            #[cfg(windows)]
            process: std::sync::Arc::new(
                crate::Process::open(std::process::id()).unwrap().unwrap(),
            ),
        }
    }

    #[test]
    fn tree_membership_checks_identity_includes_sessions_and_bounds_program_groups() {
        let members = vec![
            member(1, 0, 10, 1, 1),
            member(2, 1, 20, 1, 2),
            member(3, 99, 30, 1, 3),
            member(4, 99, 40, 4, 4),
            member(5, 1, 5, 5, 5),
        ];
        let table = ProcessTable {
            members: members
                .into_iter()
                .map(|member| (member.pid, member))
                .collect(),
            unreadable: BTreeMap::new(),
            revision: 1,
            observed_at_ms: 0,
            started_at: None,
        };
        assert!(table.tree(1, 11, true).is_none());
        let tree = table.tree(1, 10, false).unwrap();
        assert_eq!(
            (tree.cpu_ms, tree.memory_bytes, tree.process_count),
            (30, 3, 2)
        );
        let session = table.tree(1, 10, true).unwrap();
        assert_eq!(
            (session.cpu_ms, session.memory_bytes, session.process_count),
            (60, 6, 3)
        );
        assert_eq!(session.groups[0].name, "program-3");
        let mut members: Vec<_> = (1..=12)
            .map(|pid| member(pid, 0, 100, 0, u64::from(pid)))
            .collect();
        members.push(members[0].clone());
        let groups = ProcessTable::sum(members).unwrap();
        assert_eq!(
            groups.process_count, 12,
            "duplicate identities are counted only once"
        );
        assert_eq!(groups.groups.len(), 8);
        assert_eq!(groups.groups[0].memory_bytes, 12);
        assert_eq!(groups.groups[7].memory_bytes, 5);
    }

    #[test]
    fn a_child_reaped_during_the_walk_keeps_parent_cpu_without_double_counting() {
        let mut table = ProcessTable::default();
        let mut parent = member(1, 0, 10, 1, 1);
        parent.resources.as_mut().unwrap().cpu_user = Duration::from_millis(110);
        parent.reaped_cpu_ms = Some(100);
        table.members.insert(1, parent);
        table.members.insert(2, member(2, 1, 20, 1, 10));
        ProcessTable::fence(&mut table.members, &mut table.unreadable, |member| {
            if member.pid == 2 {
                crate::IdentityRead::Gone
            } else {
                crate::IdentityRead::Alive(member.start_time)
            }
        });
        let tree = table
            .tree(1, 10, false)
            .expect("a confirmed exit must not void the parent");
        assert_eq!(
            tree.cpu_ms, 110,
            "the child's 100 ms is already in the parent's reaped counter"
        );
        assert_eq!(tree.process_count, 1);
        assert_eq!(
            table.identity_alive(2, 20),
            Some(false),
            "a confirmed exit is not an unreadable live identity"
        );
    }

    #[test]
    fn identity_checks_drop_only_confirmed_exits_at_either_fence() {
        use crate::IdentityRead::{Alive, Gone, Unknown};
        for current in [Gone, Unknown, Alive(21)] {
            let mut unreadable = BTreeMap::new();
            assert!(!ProcessTable::same_identity(
                2,
                20,
                (Some(1), Some(1)),
                current,
                &mut unreadable
            ));
            assert_eq!(unreadable.contains_key(&2), current != Gone);
            let mut members = BTreeMap::from([(2, member(2, 1, 20, 1, 10))]);
            unreadable.clear();
            ProcessTable::fence(&mut members, &mut unreadable, |_| current);
            assert!(members.is_empty());
            assert_eq!(
                unreadable.contains_key(&2),
                current != Gone,
                "unknown identity and PID reuse must still reject the related tree"
            );
        }
    }

    #[test]
    fn a_child_exiting_after_parent_read_defers_cpu_until_reaped() {
        let mut table = ProcessTable::default();
        table.members.insert(1, member(1, 0, 10, 1, 1));
        table.members.insert(2, member(2, 1, 20, 1, 10));
        ProcessTable::fence(&mut table.members, &mut table.unreadable, |member| {
            if member.pid == 2 {
                crate::IdentityRead::Gone
            } else {
                crate::IdentityRead::Alive(member.start_time)
            }
        });
        assert_eq!(table.tree(1, 10, false).unwrap().cpu_ms, 10);
        let parent = table.members.get_mut(&1).unwrap();
        parent.reaped_cpu_ms = Some(100);
        parent.resources.as_mut().unwrap().cpu_user = Duration::from_millis(110);
        assert_eq!(table.tree(1, 10, false).unwrap().cpu_ms, 110);
        assert_eq!(
            table.tree(1, 10, false).unwrap().cpu_ms,
            110,
            "re-reading the same cumulative counter cannot credit another child sample"
        );
    }

    #[test]
    fn unreadable_member_never_becomes_a_confident_partial_tree() {
        let root = member(1, 0, 10, 1, 1);
        let mut child = member(2, 1, 20, 1, 2);
        child.resources = None;
        child.own_cpu_ms = None;
        child.reaped_cpu_ms = None;
        assert!(ProcessTable::sum(vec![root.clone(), child.clone()]).is_none());
        let mut table = ProcessTable::default();
        table.members.insert(1, root);
        table.members.insert(2, child);
        assert_eq!(
            table.tree_identities(1, 10, false).unwrap().len(),
            2,
            "resource uncertainty does not hide structural overlap"
        );
        assert!(table.tree(1, 10, false).is_none());
        table.members.remove(&2);
        table.unreadable.insert(2, (Some(1), Some(1)));
        assert!(table.tree(1, 10, false).is_none());
        table.unreadable.insert(2, (Some(99), Some(99)));
        assert!(table.tree(1, 10, false).is_some());
        table.unreadable.insert(2, (None, None));
        assert!(table.tree(1, 10, false).is_none());
    }
}
