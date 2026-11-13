//! A narrowed walk must answer exactly like a full walk over the same process
//! table for every pid it covers, including every tree rooted at an interest
//! pid and the uncertainty such a tree reports. Both walks replay the same
//! scripted per-process answers through [`Probe`].

use super::*;
use crate::IdentityRead::{self, Alive, Gone, Unknown};
use std::cell::{Cell, RefCell};
use std::time::Duration;

#[derive(Clone, Debug)]
struct Scripted {
    /// Absent from the listing, as if started after it was taken.
    listed: bool,
    listed_parent: Option<u32>,
    /// The answer of a per-process parent read.
    parent: Option<u32>,
    session: Option<u32>,
    listed_start: Option<u64>,
    /// Successive identity reads; the last answer repeats.
    identities: Vec<IdentityRead>,
    possibly_live: bool,
    /// Own and reaped-child CPU in milliseconds; memory equals own CPU.
    usage: Option<(u64, u64)>,
}

impl Scripted {
    fn live(parent: u32, session: u32, start: u64) -> Self {
        Self {
            listed: true,
            listed_parent: Some(parent),
            parent: Some(parent),
            session: Some(session),
            listed_start: Some(start),
            identities: vec![Alive(start)],
            possibly_live: true,
            usage: Some((start % 50 + 1, start % 7)),
        }
    }

    fn start(&self) -> u64 {
        self.identities
            .iter()
            .find_map(|read| match read {
                Alive(start) => Some(*start),
                _ => None,
            })
            .or(self.listed_start)
            .unwrap_or(7)
    }
}

type World = BTreeMap<u32, Scripted>;

struct Replay<'a> {
    world: &'a World,
    identity_reads: RefCell<BTreeMap<u32, usize>>,
    calls: Cell<u64>,
    examined: RefCell<BTreeSet<u32>>,
}

impl<'a> Replay<'a> {
    fn new(world: &'a World) -> Self {
        Self {
            world,
            identity_reads: RefCell::default(),
            calls: Cell::new(0),
            examined: RefCell::default(),
        }
    }

    fn get(&self, pid: u32) -> Option<&'a Scripted> {
        self.calls.set(self.calls.get() + 1);
        self.world.get(&pid)
    }
}

impl Probe for Replay<'_> {
    fn parent(&self, pid: u32) -> Option<u32> {
        self.get(pid)?.parent
    }

    fn session(&self, pid: u32) -> Option<u32> {
        self.get(pid)?.session
    }

    fn name(&self, pid: u32) -> String {
        self.calls.set(self.calls.get() + 1);
        format!("program-{}", pid % 5)
    }

    fn identity(&self, pid: u32) -> IdentityRead {
        self.examined.borrow_mut().insert(pid);
        let Some(scripted) = self.get(pid) else {
            return Gone;
        };
        let mut reads = self.identity_reads.borrow_mut();
        let read = reads.entry(pid).or_default();
        let answer = scripted
            .identities
            .get(*read)
            .or(scripted.identities.last())
            .copied()
            .unwrap_or(Gone);
        *read += 1;
        answer
    }

    fn possibly_live(&self, pid: u32) -> bool {
        self.get(pid).is_some_and(|scripted| scripted.possibly_live)
    }

    fn usage(&self, pid: u32) -> Option<Usage> {
        let (own, reaped) = self.get(pid)?.usage?;
        Some((
            ResourceUsage {
                memory_bytes: own,
                memory_kind: MemoryKind::ResidentSet,
                swap_bytes: None,
                cpu_user: Duration::from_millis(own),
                cpu_system: Duration::ZERO,
            },
            Duration::from_millis(reaped),
            Duration::ZERO,
        ))
    }
}

struct Walk {
    table: ProcessTable,
    calls: u64,
    examined: BTreeSet<u32>,
}

fn walk(world: &World, interest: Option<&BTreeSet<u32>>) -> Walk {
    let probe = Replay::new(world);
    let entries: Vec<Entry> = world
        .iter()
        .filter(|(_, scripted)| scripted.listed)
        .map(|(pid, scripted)| (*pid, scripted.listed_parent, None, scripted.listed_start))
        .collect();
    let table = ProcessTable::read_probed(|| Ok(entries), &probe, interest).unwrap();
    Walk {
        table,
        calls: probe.calls.get(),
        examined: probe.examined.take(),
    }
}

type Described = (
    u32,
    Option<u32>,
    u64,
    u64,
    String,
    Option<(u64, Duration)>,
    Option<u64>,
    Option<u64>,
);

fn describe(member: &ProcessMember) -> Described {
    (
        member.parent_pid,
        member.session_id,
        member.start_time,
        member.start_time_ms,
        member.name.clone(),
        member
            .resources
            .map(|usage| (usage.memory_bytes, usage.cpu_user + usage.cpu_system)),
        member.own_cpu_ms,
        member.reaped_cpu_ms,
    )
}

type Summary = (u64, u64, u64, Vec<ProcessGroup>, Vec<(u32, u64)>);

fn summary(usage: TreeUsage) -> Summary {
    (
        usage.cpu_ms,
        usage.memory_bytes,
        usage.process_count,
        usage.groups,
        usage
            .members
            .iter()
            .map(|member| (member.pid, member.start_time))
            .collect(),
    )
}

/// Compare every answer the narrowed table claims to give exactly, and
/// return how many processes it examined.
fn assert_same_answers(world: &World, interest: &BTreeSet<u32>, context: &str) -> (u64, u64) {
    let full = walk(world, None);
    let narrow = walk(world, Some(interest));
    assert!(!full.table.is_narrowed() && narrow.table.is_narrowed());
    let start = |pid: u32| world.get(&pid).map_or(7, Scripted::start);
    let mut covered: BTreeSet<u32> = interest.clone();
    covered.extend(
        world
            .keys()
            .copied()
            .chain([0, 9_999])
            .filter(|pid| narrow.table.covers(*pid)),
    );
    for pid in &covered {
        let pid = *pid;
        assert!(narrow.table.covers(pid), "{context}: {pid} is not covered");
        for start in [start(pid), start(pid) + 1, 0] {
            assert_eq!(
                full.table.identity_alive(pid, start),
                narrow.table.identity_alive(pid, start),
                "{context}: identity of {pid} at {start}"
            );
        }
        assert_eq!(
            full.table.process(pid).map(describe),
            narrow.table.process(pid).map(describe),
            "{context}: member {pid}"
        );
        // Every parent an ancestry walk can visit is covered too.
        let mut seen = BTreeSet::new();
        let mut current = narrow.table.process(pid);
        while let Some(member) = current.filter(|member| seen.insert(member.pid)) {
            assert!(
                narrow.table.covers(member.parent_pid),
                "{context}: parent {} of {} is not covered",
                member.parent_pid,
                member.pid
            );
            current = narrow.table.process(member.parent_pid);
        }
    }
    for member in narrow.table.members() {
        assert!(covered.contains(&member.pid));
    }
    for pid in interest {
        for start in [start(*pid), start(*pid) + 1] {
            for session in [false, true] {
                assert_eq!(
                    full.table.tree_identities(*pid, start, session),
                    narrow.table.tree_identities(*pid, start, session),
                    "{context}: tree identities of {pid} (session {session})"
                );
                assert_eq!(
                    full.table.tree(*pid, start, session).map(summary),
                    narrow.table.tree(*pid, start, session).map(summary),
                    "{context}: tree usage of {pid} (session {session})"
                );
            }
        }
    }
    assert!(narrow.calls <= full.calls, "{context}: narrowing read more");
    (full.calls, narrow.calls)
}

/// One module, two command roots and unrelated host processes, with every
/// attribution and uncertainty case named.
fn scenario() -> World {
    let mut world = World::new();
    world.insert(1, Scripted::live(0, 1, 1));
    // The module and its command root, which leads its own session.
    world.insert(100, Scripted::live(1, 100, 1_000));
    world.insert(101, Scripted::live(100, 101, 1_010));
    world.insert(102, Scripted::live(101, 101, 1_020));
    world.insert(103, Scripted::live(102, 101, 1_030));
    // Reparented to launchd but still in the root's session.
    world.insert(104, Scripted::live(1, 101, 1_040));
    // Left the session with setsid but is still a descendant.
    world.insert(105, Scripted::live(102, 105, 1_050));
    world.insert(115, Scripted::live(105, 105, 1_150));
    // Both reparented and left the session: no longer attributable.
    world.insert(106, Scripted::live(1, 106, 1_060));
    // Exited between its identity reads, and before the final fence.
    let mut gone = Scripted::live(102, 101, 1_070);
    gone.identities = vec![Alive(1_070), Gone];
    world.insert(107, gone);
    let mut fenced = Scripted::live(101, 101, 1_080);
    fenced.identities = vec![Alive(1_080), Alive(1_080), Gone];
    world.insert(108, fenced);
    // Exited before it was read at all.
    let mut exited = Scripted::live(101, 101, 1_090);
    exited.identities = vec![Gone];
    world.insert(109, exited);
    // Started after the listing: neither walk sees it.
    let mut late = Scripted::live(101, 101, 1_095);
    late.listed = false;
    world.insert(110, late);
    // A child older than its parent pid's current owner: that pid was reused.
    world.insert(111, Scripted::live(103, 101, 900));
    // A second root whose child is unreadable, and a third whose child's pid
    // was reused between the listing and its identity read.
    world.insert(201, Scripted::live(100, 201, 2_010));
    let mut unreadable = Scripted::live(201, 201, 2_020);
    unreadable.identities = vec![Unknown];
    world.insert(202, unreadable);
    world.insert(301, Scripted::live(100, 301, 3_010));
    let mut reused = Scripted::live(301, 301, 3_020);
    reused.listed_start = Some(3_019);
    world.insert(302, reused);
    // Unrelated host processes, including an unreadable one.
    for pid in 400..460 {
        world.insert(pid, Scripted::live(1, pid, u64::from(pid) * 10));
        world.insert(pid + 100, Scripted::live(pid, pid, u64::from(pid) * 10 + 1));
    }
    let mut stranger = Scripted::live(1, 470, 4_700);
    stranger.identities = vec![Unknown];
    world.insert(470, stranger);
    world
}

#[test]
fn narrowed_walk_matches_the_full_walk_for_named_cases() {
    let world = scenario();
    let interest = BTreeSet::from([100, 101, 201, 301]);
    let (full_calls, narrow_calls) = assert_same_answers(&world, &interest, "scenario");
    let narrow = walk(&world, Some(&interest));
    let session = narrow.table.tree_identities(101, 1_010, true).unwrap();
    let pids: BTreeSet<u32> = session.iter().map(|(pid, _)| *pid).collect();
    assert_eq!(
        pids,
        BTreeSet::from([101, 102, 103, 104, 105, 115]),
        "the session keeps its reparented member and the descendant keeps its escaper"
    );
    let tree: BTreeSet<u32> = narrow
        .table
        .tree_identities(101, 1_010, false)
        .unwrap()
        .iter()
        .map(|(pid, _)| *pid)
        .collect();
    assert_eq!(tree, BTreeSet::from([101, 102, 103, 105, 115]));
    assert!(narrow.table.tree(201, 2_010, true).is_none());
    assert!(narrow.table.tree(301, 3_010, false).is_none());
    assert_eq!(narrow.table.identity_alive(302, 3_020), None);
    assert!(narrow.table.tree(100, 1_000, false).is_none());
    assert!(
        (400..460)
            .chain(500..560)
            .all(|pid| !narrow.examined.contains(&pid) && !narrow.table.covers(pid)),
        "unrelated processes were examined"
    );
    assert!(!narrow.examined.contains(&106) && !narrow.examined.contains(&470));
    assert!(
        narrow_calls * 2 < full_calls,
        "{narrow_calls} of {full_calls}"
    );

    // A process whose parent no read can establish could belong to any tree.
    let mut orphaned = world.clone();
    let mut unknown = Scripted::live(1, 480, 4_800);
    unknown.listed_parent = None;
    unknown.parent = None;
    orphaned.insert(480, unknown);
    assert_same_answers(&orphaned, &interest, "unknown parent");
    assert!(walk(&orphaned, Some(&interest))
        .table
        .tree(101, 1_010, false)
        .is_none());
    // An unknown session can belong to any session.
    let mut sessionless = world;
    sessionless.get_mut(&470).unwrap().session = None;
    assert_same_answers(&sessionless, &interest, "unknown session");
    assert!(walk(&sessionless, Some(&interest))
        .table
        .tree(101, 1_010, true)
        .is_none());
    assert!(walk(&sessionless, Some(&interest))
        .table
        .tree(101, 1_010, false)
        .is_some());
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % bound
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

fn random_world(rng: &mut Rng) -> World {
    let count = 30 + rng.below(50) as u32;
    let mut world = World::new();
    world.insert(1, Scripted::live(0, 1, 1));
    let mut leaders = vec![1];
    for pid in 2..=count {
        let parent = if rng.chance(10) {
            1
        } else {
            1 + rng.below(u64::from(pid - 1)) as u32
        };
        let parent_session = world[&parent].session.unwrap_or(1);
        let session = if rng.chance(15) {
            leaders.push(pid);
            pid
        } else if rng.chance(10) {
            leaders[rng.below(leaders.len() as u64) as usize]
        } else {
            parent_session
        };
        let start = if rng.chance(5) {
            5
        } else {
            u64::from(pid) * 10
        };
        let mut scripted = Scripted::live(parent, session, start);
        if rng.chance(3) {
            scripted.session = None;
        }
        if rng.chance(3) {
            scripted.listed_parent = None;
            if rng.chance(50) {
                scripted.parent = None;
            }
        }
        if rng.chance(5) {
            scripted.listed_start = Some(start + 1);
        } else if rng.chance(10) {
            scripted.listed_start = None;
        }
        scripted.identities = match rng.below(100) {
            0..6 => vec![Gone],
            6..10 => vec![Alive(start), Gone],
            10..14 => vec![Alive(start), Alive(start), Gone],
            14..17 => vec![Unknown],
            17..19 => vec![Alive(start), Unknown],
            19..21 => vec![Alive(start), Alive(start + 3)],
            _ => vec![Alive(start)],
        };
        scripted.possibly_live = !rng.chance(30);
        if rng.chance(5) {
            scripted.usage = None;
        }
        scripted.listed = !rng.chance(3);
        world.insert(pid, scripted);
    }
    world
}

#[test]
fn narrowed_walk_matches_the_full_walk_for_random_tables() {
    let mut rng = Rng(0x5eed);
    let (mut full_calls, mut narrow_calls) = (0, 0);
    for round in 0..400 {
        let world = random_world(&mut rng);
        let count = world.len() as u64;
        let mut pick = || 1 + rng.below(count + 2) as u32;
        let leader = world
            .iter()
            .find(|(pid, scripted)| **pid > 1 && scripted.session == Some(**pid))
            .map_or(2, |(pid, _)| *pid);
        for interest in [
            BTreeSet::from([pick()]),
            BTreeSet::from([pick(), pick(), pick()]),
            BTreeSet::from([leader]),
            BTreeSet::from([leader, pick(), 9_999]),
        ] {
            let (full, narrow) =
                assert_same_answers(&world, &interest, &format!("round {round} {interest:?}"));
            full_calls += full;
            narrow_calls += narrow;
        }
    }
    assert!(narrow_calls < full_calls);
}

/// Counts per-process reads to estimate a native walk's system calls.
#[cfg(target_os = "macos")]
struct Counting<P> {
    inner: P,
    calls: [Cell<u64>; 6],
}

#[cfg(target_os = "macos")]
impl<P: Probe> Counting<P> {
    fn new(inner: P) -> Self {
        Self {
            inner,
            calls: Default::default(),
        }
    }

    fn count(&self, index: usize) {
        self.calls[index].set(self.calls[index].get() + 1);
    }

    fn counts(&self) -> [u64; 6] {
        self.calls.each_ref().map(Cell::get)
    }
}

#[cfg(target_os = "macos")]
impl<P: Probe> Probe for Counting<P> {
    fn parent(&self, pid: u32) -> Option<u32> {
        self.count(0);
        self.inner.parent(pid)
    }
    fn session(&self, pid: u32) -> Option<u32> {
        self.count(1);
        self.inner.session(pid)
    }
    fn name(&self, pid: u32) -> String {
        self.count(2);
        self.inner.name(pid)
    }
    fn identity(&self, pid: u32) -> IdentityRead {
        self.count(3);
        self.inner.identity(pid)
    }
    fn possibly_live(&self, pid: u32) -> bool {
        self.count(4);
        self.inner.possibly_live(pid)
    }
    fn usage(&self, pid: u32) -> Option<Usage> {
        self.count(5);
        self.inner.usage(pid)
    }
}

/// Times the former full-table walk, which the daemon took for every
/// agent-command declaration, against a narrowed walk for a realistic target: this test process as the module,
/// with three `sleep` children as command roots. Reads only; never signals
/// anything but its own children. Run with `--ignored --nocapture`.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "measures this machine's process table"]
fn native_narrowed_walk_cost() {
    let mut children: Vec<_> = (0..3)
        .map(|index| {
            let mut command = std::process::Command::new("sleep");
            command.arg("30");
            if index == 0 {
                // Agent command roots usually lead their own session, which
                // makes the walk read every listed process's session.
                use std::os::unix::process::CommandExt;
                // SAFETY: setsid is async-signal-safe and touches no memory.
                #[allow(unsafe_code)]
                unsafe {
                    command.pre_exec(|| {
                        libc::setsid();
                        Ok(())
                    });
                }
            }
            command.spawn().unwrap()
        })
        .collect();
    let mut interest = BTreeSet::from([std::process::id()]);
    interest.extend(children.iter().map(std::process::Child::id));
    // Per-read system calls on macOS: parent 1 (`proc_pidinfo`, plus a
    // `sysctl` fallback when a protected process refuses it), session 1,
    // name 1, identity 1, liveness 1, usage 2 (`proc_pid_rusage`, then a
    // `sysctl` check that the process is not a zombie).
    let weights = [1, 1, 1, 1, 1, 2];
    let estimate = |counts: [u64; 6], listing: u64| {
        listing
            + counts
                .iter()
                .zip(weights)
                .map(|(count, weight)| count * weight)
                .sum::<u64>()
    };
    let mut times: [Vec<f64>; 3] = Default::default();
    let mut report = Vec::new();
    for round in 0..15 {
        let mut legacy_names = 0;
        let legacy = Counting::new(Native);
        let began = std::time::Instant::now();
        let table = ProcessTable::read_probed(
            || {
                let entries = legacy_entries()?;
                legacy_names = entries.len() as u64;
                Ok(entries)
            },
            &legacy,
            None,
        )
        .unwrap();
        times[0].push(began.elapsed().as_secs_f64() * 1e3);
        let legacy_members = table.members().count();
        let full = Counting::new(Native);
        let began = std::time::Instant::now();
        let table = ProcessTable::read_probed(entries, &full, None).unwrap();
        times[1].push(began.elapsed().as_secs_f64() * 1e3);
        let full_members = table.members().count();
        let narrow = Counting::new(Native);
        let began = std::time::Instant::now();
        let table = ProcessTable::read_probed(entries, &narrow, Some(&interest)).unwrap();
        times[2].push(began.elapsed().as_secs_f64() * 1e3);
        let measured_roots = interest
            .iter()
            .filter(|pid| {
                table
                    .process(**pid)
                    .is_some_and(|root| table.tree(**pid, root.start_time, true).is_some())
            })
            .count();
        if round == 0 {
            report.push(format!(
                "old walk: members={legacy_members} reads={:?} syscalls~{}",
                legacy.counts(),
                // proc_listallpids twice, then proc_name per pid.
                estimate(legacy.counts(), 2 + legacy_names)
            ));
            report.push(format!(
                "full walk from one listing: members={full_members} reads={:?} syscalls~{}",
                full.counts(),
                estimate(full.counts(), 2)
            ));
            report.push(format!(
                "narrowed walk: members={} measured_roots={measured_roots}/{} reads={:?} syscalls~{}",
                table.members().count(),
                interest.len(),
                narrow.counts(),
                estimate(narrow.counts(), 2)
            ));
        }
    }
    for child in &mut children {
        let _ = child.kill();
        let _ = child.wait();
    }
    for (label, times) in ["old", "full", "narrowed"].iter().zip(&mut times) {
        times.sort_by(f64::total_cmp);
        report.push(format!(
            "{label}: median_ms={:.2} min_ms={:.2} max_ms={:.2}",
            times[times.len() / 2],
            times[0],
            times[times.len() - 1]
        ));
    }
    println!("reads are [parent, session, name, identity, liveness, usage]");
    for line in report {
        println!("{line}");
    }
}
