//! Read-only process-table snapshots. No process is stopped or signalled.

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use crate::{MemoryKind, ResourceUsage};

#[cfg(feature = "test-support")]
std::thread_local! {
    static TEST_WALL_CLOCK: std::cell::Cell<Option<std::time::SystemTime>> = const {
        std::cell::Cell::new(None)
    };
}

fn wall_clock_now() -> std::time::SystemTime {
    #[cfg(feature = "test-support")]
    if let Some(now) = TEST_WALL_CLOCK.get() {
        return now;
    }
    std::time::SystemTime::now()
}

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
///
/// A table read with [`ProcessTable::read_for`] is narrowed: only the
/// processes its interest set can reach were examined, and [`Self::covers`]
/// says which pids it answers for exactly as a full walk would.
#[derive(Debug, Clone, Default)]
pub struct ProcessTable {
    members: BTreeMap<u32, ProcessMember>,
    unreadable: BTreeMap<u32, (Option<u32>, Option<u32>)>,
    revision: u64,
    observed_at_ms: u64,
    started_at: Option<std::time::Instant>,
    focus: Option<std::sync::Arc<Focus>>,
}

/// The pids a narrowed walk examined, and every pid its listing contained.
/// A pid the listing did not contain is absent from a full walk too.
#[derive(Debug, Default)]
struct Focus {
    selected: BTreeSet<u32>,
    listed: BTreeSet<u32>,
}

impl ProcessTable {
    /// Override only this thread's snapshot wall clock, restoring it on unwind.
    /// Native process creation times and the monotonic clock are not changed.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn with_test_wall_clock<T>(now: std::time::SystemTime, operation: impl FnOnce() -> T) -> T {
        struct Restore(Option<std::time::SystemTime>);
        impl Drop for Restore {
            fn drop(&mut self) {
                TEST_WALL_CLOCK.set(self.0);
            }
        }
        let _restore = Restore(TEST_WALL_CLOCK.replace(Some(now)));
        operation()
    }

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
            focus: None,
        }
    }
    /// Read all visible processes once. Unreadable identities are retained as
    /// uncertainty, so a related tree cannot silently become a partial sum.
    pub fn read() -> io::Result<Self> {
        Self::read_entries(entries)
    }

    /// Read only what `interest` can reach, answering exactly as [`Self::read`]
    /// would for every pid [`Self::covers`] accepts.
    ///
    /// One listing supplies every process's parent and start. The per-process
    /// reads (identity, session, resources and the exit fence) then cover only:
    /// the interest pids; every listed process whose session is led by an
    /// interest pid, or whose session or parent is unknown; all their
    /// descendants; and every ancestor of all of those. That is every process
    /// a tree rooted at an interest pid can select or be made uncertain by,
    /// with or without its session, and every parent an ancestry walk from a
    /// covered process can visit. Windows reads the full table.
    pub fn read_for(interest: &BTreeSet<u32>) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Self::read_probed(entries, &Native, Some(interest))
        }
        #[cfg(not(unix))]
        {
            let _ = interest;
            Self::read()
        }
    }

    /// Whether queries about `pid` match a full walk. Always true for a full
    /// table; for a narrowed one, true for examined pids and for pids the
    /// walk's listing did not contain at all.
    pub fn covers(&self, pid: u32) -> bool {
        self.focus
            .as_ref()
            .is_none_or(|focus| focus.selected.contains(&pid) || !focus.listed.contains(&pid))
    }

    /// Whether this table examined only part of the host's processes.
    pub fn is_narrowed(&self) -> bool {
        self.focus.is_some()
    }

    fn begin() -> (u64, u64, std::time::Instant) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let revision = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let observed_at_ms = wall_clock_now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
            .unwrap_or(0);
        (revision, observed_at_ms, std::time::Instant::now())
    }

    fn read_entries(enumerate: impl FnOnce() -> io::Result<Vec<Entry>>) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Self::read_probed(enumerate, &Native, None)
        }
        #[cfg(windows)]
        {
            Self::read_windows(enumerate)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = enumerate;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "process tables unavailable",
            ))
        }
    }

    /// The walk itself. With `interest`, only the processes [`focus`] selects
    /// get the per-process reads; each of those gets exactly the reads, in the
    /// order, that a full walk gives it.
    #[cfg(unix)]
    fn read_probed(
        enumerate: impl FnOnce() -> io::Result<Vec<Entry>>,
        probe: &impl Probe,
        interest: Option<&BTreeSet<u32>>,
    ) -> io::Result<Self> {
        let (revision, observed_at_ms, started_at) = Self::begin();
        let entries = enumerate()?;
        let mut known = Known::default();
        let focus = interest.map(|interest| focus(&entries, interest, probe, &mut known));
        let mut members = BTreeMap::new();
        let mut unreadable = BTreeMap::new();
        for (pid, parent, name, expected_start) in entries {
            if focus
                .as_ref()
                .is_some_and(|focus| !focus.selected.contains(&pid))
            {
                continue;
            }
            let parent = parent.or_else(|| {
                known
                    .parents
                    .remove(&pid)
                    .unwrap_or_else(|| probe.parent(pid))
            });
            let session_id = known
                .sessions
                .remove(&pid)
                .unwrap_or_else(|| probe.session(pid));
            let start = match probe.identity(pid) {
                crate::IdentityRead::Alive(start) => Some(start),
                crate::IdentityRead::Gone => continue,
                crate::IdentityRead::Unknown => None,
            };
            let Some(start_time) =
                start.filter(|start| expected_start.is_none_or(|expected| expected == *start))
            else {
                if probe.possibly_live(pid) {
                    unreadable.insert(pid, (parent, session_id));
                }
                continue;
            };
            let Some(start_time_ms) = crate::start_time_ms_from_native(start_time) else {
                unreadable.insert(pid, (parent, session_id));
                continue;
            };
            let Some(parent_pid) = parent else {
                unreadable.insert(pid, (None, session_id));
                continue;
            };
            let name = name.unwrap_or_else(|| probe.name(pid));
            let (resources, own_cpu_ms, reaped_cpu_ms) = probe.usage(pid).map_or(
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
            if !Self::same_identity(
                pid,
                start_time,
                (Some(parent_pid), session_id),
                probe.identity(pid),
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
                },
            );
        }
        // A reaped child's earlier sample can overlap a later parent's reaped
        // counter. Drop confirmed exits, leaving recovery to that counter, but
        // reject unknown identities rather than publishing a partial live sum.
        Self::fence(&mut members, &mut unreadable, |member| {
            probe.identity(member.pid)
        });
        Ok(Self {
            members,
            unreadable,
            revision,
            observed_at_ms,
            started_at: Some(started_at),
            focus: focus.map(std::sync::Arc::new),
        })
    }

    #[cfg(windows)]
    fn read_windows(enumerate: impl FnOnce() -> io::Result<Vec<Entry>>) -> io::Result<Self> {
        let (revision, observed_at_ms, started_at) = Self::begin();
        let mut members = BTreeMap::new();
        let mut unreadable = BTreeMap::new();
        let entries = enumerate()?;
        let listed_at: BTreeMap<u32, usize> = entries
            .iter()
            .enumerate()
            .map(|(index, (pid, ..))| (*pid, index))
            .collect();
        for (index, (pid, parent, name, expected_start)) in entries.into_iter().enumerate() {
            let parent = parent.or_else(|| crate::parent_pid(pid));
            let parent = live_parent(parent, index, &listed_at);
            let session_id = None;
            let Some(process) = crate::Process::open(pid).ok().flatten() else {
                unreadable.insert(pid, (parent, session_id));
                continue;
            };
            let start = process.observe().map(|observation| observation.start_time);
            let Some(start_time) =
                start.filter(|start| expected_start.is_none_or(|expected| expected == *start))
            else {
                if possibly_live(pid) {
                    unreadable.insert(pid, (parent, session_id));
                }
                continue;
            };
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
            let resources = process.resource_usage();
            let own_cpu_ms = resources.map(|usage| {
                u64::try_from((usage.cpu_user + usage.cpu_system).as_millis()).unwrap_or(u64::MAX)
            });
            let reaped_cpu_ms = resources.map(|_| 0);
            let current_start = process
                .observe()
                .map_or(crate::IdentityRead::Unknown, |observation| {
                    crate::IdentityRead::Alive(observation.start_time)
                });
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
                    name: name.unwrap_or_default(),
                    resources,
                    own_cpu_ms,
                    reaped_cpu_ms,
                    process: std::sync::Arc::new(process),
                },
            );
        }
        // A reaped child's earlier sample can overlap a later parent's reaped
        // counter. Drop confirmed exits, leaving recovery to that counter, but
        // reject unknown identities rather than publishing a partial live sum.
        Self::fence(&mut members, &mut unreadable, |member| {
            member
                .process
                .observe()
                .map_or(crate::IdentityRead::Unknown, |observation| {
                    crate::IdentityRead::Alive(observation.start_time)
                })
        });
        Ok(Self {
            members,
            unreadable,
            revision,
            observed_at_ms,
            started_at: Some(started_at),
            focus: None,
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

#[cfg(windows)]
fn possibly_live(_pid: u32) -> bool {
    true
}

/// Windows never reparents: a process keeps the pid of the parent that
/// created it, even after that parent exits and the pid is given to a new
/// process. Otherwise a new process that reuses such a pid inherits unrelated
/// children: an unopenable one (csrss.exe and wininit.exe name an exited
/// smss.exe) makes every tree rooted at the new process unknown, and a
/// readable one makes the new process look like an ancestor.
///
/// Creation order, not start time, tells a real parent from a reused pid.
/// Start times are wall-clock times, and a clock stepped back after boot
/// makes boot processes look younger than processes started since. The
/// native listing walks the kernel's active process list
/// (`PsActiveProcessHead`), which appends each process when it is created, so
/// a real parent is always listed before its child. A listed parent that
/// appears after the child is a later process holding a reused pid, and the
/// child is recorded with no live parent (0, as for System). This ordering is
/// not documented; `the_native_listing_is_in_creation_order` fails if Windows
/// stops providing it.
#[cfg(windows)]
fn live_parent(
    parent: Option<u32>,
    child_at: usize,
    listed_at: &BTreeMap<u32, usize>,
) -> Option<u32> {
    let parent = parent?;
    let reused = listed_at
        .get(&parent)
        .is_some_and(|parent_at| *parent_at > child_at);
    Some(if reused { 0 } else { parent })
}

/// One listed process: pid, parent, program name and native start, each
/// `None` when the listing did not supply it. A missing parent or name is
/// read per process; a missing start is not compared against.
type Entry = (u32, Option<u32>, Option<String>, Option<u64>);

/// Own CPU and memory, then reaped-child user and system CPU.
#[cfg(unix)]
type Usage = (ResourceUsage, std::time::Duration, std::time::Duration);

/// The per-process reads of a Unix walk. Tests substitute scripted answers
/// to compare a narrowed walk with a full one over the same process table.
#[cfg(unix)]
trait Probe {
    fn parent(&self, pid: u32) -> Option<u32>;
    fn session(&self, pid: u32) -> Option<u32>;
    fn name(&self, pid: u32) -> String;
    fn identity(&self, pid: u32) -> crate::IdentityRead;
    fn possibly_live(&self, pid: u32) -> bool;
    fn usage(&self, pid: u32) -> Option<Usage>;
}

#[cfg(unix)]
struct Native;

#[cfg(unix)]
impl Probe for Native {
    fn parent(&self, pid: u32) -> Option<u32> {
        crate::parent_pid(pid)
    }

    fn session(&self, pid: u32) -> Option<u32> {
        // SAFETY: getsid is a read-only query of a numeric process.
        #[allow(unsafe_code)]
        let session = unsafe { libc::getsid(pid as libc::pid_t) };
        u32::try_from(session).ok()
    }

    fn name(&self, pid: u32) -> String {
        #[cfg(target_os = "macos")]
        {
            program_name(pid)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = pid;
            String::new()
        }
    }

    fn identity(&self, pid: u32) -> crate::IdentityRead {
        crate::read_identity(pid)
    }

    fn possibly_live(&self, pid: u32) -> bool {
        possibly_live(pid)
    }

    fn usage(&self, pid: u32) -> Option<Usage> {
        #[cfg(target_os = "linux")]
        {
            crate::linux::resource_usage_parts(pid)
        }
        #[cfg(target_os = "macos")]
        {
            crate::macos::resource_usage_parts(pid)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            // Without separate reaped-child counters, all CPU is the process's own.
            crate::resource_usage(pid)
                .map(|usage| (usage, std::time::Duration::ZERO, std::time::Duration::ZERO))
        }
    }
}

/// Parents and sessions a narrowed walk read while choosing its focus. The
/// walk itself uses these answers instead of asking again, so each examined
/// process gets one parent and one session read, as in a full walk.
#[cfg(unix)]
#[derive(Default)]
struct Known {
    parents: BTreeMap<u32, Option<u32>>,
    sessions: BTreeMap<u32, Option<u32>>,
}

/// Choose the processes a narrowed walk examines; see [`ProcessTable::read_for`].
///
/// A tree query rooted at `root` selects `root`, then repeatedly every member
/// whose parent is selected or, with its session, whose session id is
/// `root` when `root` leads its own session. It is made uncertain by an
/// unreadable process that is selected, whose parent is selected or unknown,
/// or, with the session, whose session is `root` or unknown. Every such
/// process is an interest pid, a session member of an interest pid that
/// leads its session, a process with an unknown session or parent, or a
/// descendant of one of those, so that set is examined. Adding all of their
/// ancestors keeps every parent lookup and ancestry walk from an examined
/// process exact too.
#[cfg(unix)]
fn focus(
    entries: &[Entry],
    interest: &BTreeSet<u32>,
    probe: &impl Probe,
    known: &mut Known,
) -> Focus {
    let mut parents = BTreeMap::new();
    for (pid, parent, ..) in entries {
        let parent = match parent {
            Some(parent) => Some(*parent),
            None => *known
                .parents
                .entry(*pid)
                .or_insert_with(|| probe.parent(*pid)),
        };
        parents.insert(*pid, parent);
    }
    let mut children = BTreeMap::<u32, Vec<u32>>::new();
    for (pid, parent) in &parents {
        if let Some(parent) = parent {
            children.entry(*parent).or_default().push(*pid);
        }
    }
    let mut seeds: Vec<u32> = interest.iter().copied().collect();
    seeds.extend(
        parents
            .iter()
            .filter(|(_, parent)| parent.is_none())
            .map(|(pid, _)| *pid),
    );
    let mut session = |pid: u32| {
        *known
            .sessions
            .entry(pid)
            .or_insert_with(|| probe.session(pid))
    };
    let leaders: BTreeSet<u32> = interest
        .iter()
        .copied()
        .filter(|pid| parents.contains_key(pid))
        .filter(|pid| session(*pid) == Some(*pid))
        .collect();
    if !leaders.is_empty() {
        for pid in parents.keys() {
            if session(*pid).is_none_or(|session| leaders.contains(&session)) {
                seeds.push(*pid);
            }
        }
    }
    let mut selected = BTreeSet::new();
    while let Some(pid) = seeds.pop() {
        if selected.insert(pid) {
            seeds.extend(children.get(&pid).into_iter().flatten().copied());
        }
    }
    let reached: Vec<u32> = selected.iter().copied().collect();
    for pid in reached {
        let mut current = parents.get(&pid).copied().flatten();
        while let Some(parent) = current.filter(|parent| parents.contains_key(parent)) {
            if !selected.insert(parent) {
                break;
            }
            current = parents.get(&parent).copied().flatten();
        }
    }
    Focus {
        selected,
        listed: parents.into_keys().collect(),
    }
}

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
            entries.push((pid, None, Some(String::new()), None));
            continue;
        };
        let Some((name, rest)) = stat
            .split_once('(')
            .and_then(|(_, tail)| tail.rsplit_once(')'))
        else {
            entries.push((pid, None, Some(String::new()), None));
            continue;
        };
        let parent = rest.split_whitespace().nth(1).and_then(|s| s.parse().ok());
        let start = rest
            .split_whitespace()
            .nth(19)
            .and_then(|value| value.parse().ok());
        entries.push((pid, parent, Some(name.to_owned()), start));
    }
    Ok(entries)
}

/// One `sysctl(KERN_PROC_ALL)` lists every process with its parent and
/// start. Names come from `proc_name` later, and only for processes a walk
/// keeps, because the listing's 16-byte command name is shorter.
#[cfg(target_os = "macos")]
fn entries() -> io::Result<Vec<Entry>> {
    Ok(crate::macos::process_list()?
        .into_iter()
        .map(|(pid, parent, start)| (pid, parent, None, start))
        .collect())
}

#[cfg(target_os = "macos")]
fn program_name(pid: u32) -> String {
    #[allow(unsafe_code)]
    unsafe extern "C" {
        fn proc_name(pid: libc::c_int, buffer: *mut std::ffi::c_void, size: u32) -> libc::c_int;
    }
    let mut name = [0_u8; 1024];
    // SAFETY: name is initialized, writable and its length is correct.
    #[allow(unsafe_code)]
    let length = unsafe {
        proc_name(
            pid as libc::c_int,
            name.as_mut_ptr().cast(),
            name.len() as u32,
        )
    };
    let length = usize::try_from(length).unwrap_or(0).min(name.len());
    String::from_utf8_lossy(&name[..length])
        .trim_end_matches('\0')
        .to_owned()
}

/// The listing a walk used before `KERN_PROC_ALL`: every pid from
/// `proc_listallpids` with its `proc_name`, and the parent and start left to
/// the per-process reads. Kept to measure the old walk's cost.
#[cfg(all(test, target_os = "macos"))]
fn legacy_entries() -> io::Result<Vec<Entry>> {
    #[allow(unsafe_code)]
    unsafe extern "C" {
        fn proc_listallpids(buffer: *mut std::ffi::c_void, size: libc::c_int) -> libc::c_int;
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
    if count <= 0 || count as usize >= pids.len() {
        return Err(io::Error::last_os_error());
    }
    Ok(pids
        .into_iter()
        .take(count as usize)
        .filter(|pid| *pid > 0)
        .map(|pid| (pid as u32, None, Some(program_name(pid as u32)), None))
        .collect())
}

#[cfg(windows)]
fn entries() -> io::Result<Vec<Entry>> {
    crate::windows::process_entries().map(|entries| {
        entries
            .into_iter()
            .map(|(pid, parent, name, start)| (pid, parent, Some(name), Some(start)))
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

#[cfg(all(test, unix))]
mod narrowing_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[cfg(windows)]
    #[test]
    fn wall_clock_rollback_keeps_the_live_process_in_the_table() {
        let pid = std::process::id();
        let process = crate::Process::open(pid).unwrap().unwrap();
        let start = process.observe().unwrap().start_time;
        let table = ProcessTable::with_test_wall_clock(std::time::UNIX_EPOCH, || {
            ProcessTable::read().unwrap()
        });
        assert_eq!(process.observe().unwrap().start_time, start);
        assert!(
            !table.unreadable.contains_key(&pid),
            "the readable live process must not be classified as unreadable"
        );
        assert_eq!(
            table.process(pid).map(|member| member.start_time),
            Some(start),
            "a wall clock earlier than process creation must not hide a live identity"
        );
    }

    #[cfg(windows)]
    #[test]
    fn reused_enumerated_pid_is_retained_as_uncertainty() {
        let pid = std::process::id();
        let process = crate::Process::open(pid).unwrap().unwrap();
        let start = process.observe().unwrap().start_time;
        let mut rows = entries().unwrap();
        let row = rows.iter_mut().find(|row| row.0 == pid).unwrap();
        assert_eq!(row.3, Some(start));
        row.3 = Some(start - 1);
        let table = ProcessTable::read_entries(|| Ok(rows)).unwrap();
        assert_eq!(process.observe().unwrap().start_time, start);
        assert!(table.process(pid).is_none());
        assert!(table.unreadable.contains_key(&pid));
        assert_eq!(table.identity_alive(pid, start), None);
    }

    #[cfg(windows)]
    struct Kill(std::process::Child);

    #[cfg(windows)]
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A process that waits on its open stdin until killed.
    #[cfg(windows)]
    fn waiting_process() -> Kill {
        Kill(
            std::process::Command::new("cmd.exe")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        )
    }

    /// Stale-parent detection relies on the native listing being in creation
    /// order, which Windows does not document. This fails if that changes.
    #[cfg(windows)]
    #[test]
    fn the_native_listing_is_in_creation_order() {
        let first = waiting_process();
        let second = waiting_process();
        let (first, second) = (first.0.id(), second.0.id());
        let mut rows = entries().unwrap();
        let at = |pid| rows.iter().position(|row| row.0 == pid).unwrap();
        assert!(at(std::process::id()) < at(first));
        assert!(
            at(first) < at(second),
            "a process started later is listed later"
        );
        // Cross the two links in the same real listing: only the one naming
        // the earlier process stays.
        for row in &mut rows {
            if row.0 == first {
                row.1 = Some(second);
            } else if row.0 == second {
                row.1 = Some(first);
            }
        }
        let table = ProcessTable::read_entries(|| Ok(rows)).unwrap();
        assert_eq!(table.process(second).unwrap().parent_pid, first);
        assert_eq!(table.process(first).unwrap().parent_pid, 0);
    }

    #[cfg(windows)]
    #[test]
    fn a_listed_parent_that_appears_after_its_child_is_not_its_parent() {
        // The new process plays one that reuses a pid older processes still
        // name as their parent, as csrss.exe and wininit.exe name an exited
        // smss.exe.
        let root = waiting_process();
        let root_pid = root.0.id();
        let root_start = crate::start_time(root_pid).unwrap();
        let own = std::process::id();
        let own_start = crate::start_time(own).unwrap();
        // No process holds this pid, so opening it fails and the walk keeps
        // it as unreadable, like a protected system process.
        let unopenable = u32::MAX - 3;
        let unopenable_row = |start| {
            (
                unopenable,
                Some(root_pid),
                Some("csrss.exe".into()),
                Some(start),
            )
        };
        let root_row = (
            root_pid,
            Some(own),
            Some("cmd.exe".into()),
            Some(root_start),
        );
        // Listed before the root, yet with a later start time, as boot
        // processes look after the clock steps back: order decides.
        let stale = ProcessTable::read_entries(|| {
            Ok(vec![
                (
                    own,
                    Some(root_pid),
                    Some("older.exe".into()),
                    Some(own_start),
                ),
                unopenable_row(root_start + 1),
                root_row.clone(),
            ])
        })
        .unwrap();
        assert_eq!(
            stale.process(own).map(|member| member.parent_pid),
            Some(0),
            "a readable process listed before its parent has no live parent"
        );
        assert_eq!(stale.unreadable.get(&unopenable), Some(&(Some(0), None)));
        assert_eq!(
            stale.process(root_pid).map(|member| member.parent_pid),
            Some(own),
            "a parent listed first stays linked"
        );
        assert_eq!(
            stale.tree_identities(root_pid, root_start, true),
            Some(BTreeSet::from([(root_pid, root_start)])),
            "an older unreadable process cannot make the new root's tree unknown"
        );
        // An unreadable process listed after the root, which the root may
        // have created, still does, even with an earlier start time.
        let child = ProcessTable::read_entries(|| {
            Ok(vec![
                (own, None, Some("older.exe".into()), Some(own_start)),
                root_row.clone(),
                unopenable_row(root_start - 1),
            ])
        })
        .unwrap();
        assert_eq!(
            child.unreadable.get(&unopenable),
            Some(&(Some(root_pid), None))
        );
        assert_eq!(child.tree_identities(root_pid, root_start, true), None);
    }

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
            focus: None,
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
