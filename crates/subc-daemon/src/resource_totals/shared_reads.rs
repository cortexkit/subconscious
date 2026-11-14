//! Which processes a walk must examine, and sharing one walk between
//! declarations that wait for it.
//!
//! A narrowed [`ProcessTable`] answers exactly only for the pids it covers.
//! Every query accounting makes names a pid held in a module's state, a
//! supervised module or container process, or a declared root, so collecting
//! those before a walk and checking coverage after it keeps every answer
//! identical to a full walk.

use super::*;
use std::sync::Condvar;

impl Module {
    /// Every pid this module's accounting may ask a process table about:
    /// liveness checks, trees rooted at it (sessions included) and member
    /// lookups. Ancestors need no entry: a narrowed walk covers the ancestors
    /// of everything it examines.
    pub(super) fn interest(&self, pids: &mut BTreeSet<u32>) {
        pids.extend(self.pid);
        for (root, command) in &self.roots {
            pids.insert(root.pid);
            pids.extend(command.members.iter().map(|(pid, _)| *pid));
            pids.extend(command.previous_inside.iter().map(|(pid, _)| *pid));
            pids.extend(command.cpu_floor.identities().map(|(pid, _)| pid));
            #[cfg(windows)]
            {
                pids.extend(command.fixed_inside.iter().map(|(pid, _)| *pid));
                pids.extend(command.fixed_outside.iter().map(|(pid, _)| *pid));
                pids.extend(command.member_handles.keys().map(|(pid, _)| *pid));
                pids.extend(command.pinned_cpu.iter().map(|((pid, _), _)| *pid));
                pids.extend(command.exited_at.keys().map(|(pid, _)| *pid));
            }
        }
        self.pending_pids(pids);
        pids.extend(self.omitted.keys().map(|root| root.pid));
        pids.extend(self.finished_live.keys().map(|root| root.pid));
        pids.extend(self.cpu_floor.identities().map(|(pid, _)| pid));
        pids.extend(self.cpu_model.identities().map(|(pid, _)| pid));
        pids.extend(self.group_credit.identities().map(|(pid, _)| pid));
        self.declaration_loss.root_pids(pids);
    }
}

impl Tracker {
    /// The pids every module's accounting may ask about, or `None` when the
    /// state is busy, in which case only a full walk is safe.
    pub(super) fn interest(&self) -> Option<BTreeSet<u32>> {
        let state = self.state.try_lock().ok()?;
        let pids = self.state_interest(&state);
        Some(pids)
    }

    /// The pids `state`'s modules and every module's lost roots may be
    /// asked about.
    pub(super) fn state_interest(&self, state: &State) -> BTreeSet<u32> {
        let mut pids = BTreeSet::new();
        for module in state.modules.values() {
            module.interest(&mut pids);
        }
        for losses in self
            .declaration_losses
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
        {
            losses.root_pids(&mut pids);
        }
        pids
    }
}

/// Container members are read before the walk so it examines them. A process
/// that joins the container during the walk can still be missing; its tree
/// reading is then unavailable, as when it joins after a full walk.
pub(super) fn container_pids(target: &crate::subtree_resources::Target, pids: &mut BTreeSet<u32>) {
    if target.has_container() {
        pids.extend(target.process_ids().into_iter().flatten());
    }
}

/// The pids a declaration's validation asks about beyond its module's state.
pub(super) fn declaration_pids(module_pid: u32, batch: &Declaration, pids: &mut BTreeSet<u32>) {
    pids.insert(module_pid);
    pids.extend(batch.started.iter().map(|root| root.pid));
    pids.extend(batch.known.iter().flatten().map(|root| root.pid));
    pids.extend(batch.finished.iter().map(|root| root.pid));
}

pub(super) fn covers(table: &ProcessTable, pids: &BTreeSet<u32>) -> bool {
    pids.iter().all(|pid| table.covers(*pid))
}

/// One walk at a time, shared by everyone who asked before it began.
///
/// A caller joins the next walk that has not started, so the table it gets
/// always began after it asked: a root started just before cannot be missing
/// because of an older walk. Callers that arrive while a walk runs wait for
/// it to finish, then share the following one, which reads the union of
/// their interests.
pub(super) struct SharedReads<T> {
    queue: Mutex<ReadQueue<T>>,
    done: Condvar,
}

struct ReadQueue<T> {
    reading: bool,
    /// The walk the next caller joins.
    next: u64,
    /// Union of the joined callers' interests; `None` once any caller needs
    /// a full walk.
    wanted: Option<BTreeSet<u32>>,
    waiting: BTreeMap<u64, usize>,
    results: BTreeMap<u64, T>,
}

impl<T> Default for SharedReads<T> {
    fn default() -> Self {
        Self {
            queue: Mutex::new(ReadQueue {
                reading: false,
                next: 0,
                wanted: Some(BTreeSet::new()),
                waiting: BTreeMap::new(),
                results: BTreeMap::new(),
            }),
            done: Condvar::new(),
        }
    }
}

impl<T> std::fmt::Debug for SharedReads<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SharedReads")
            .finish_non_exhaustive()
    }
}

impl<T: Clone + Default> SharedReads<T> {
    /// Join the next walk not yet begun; whoever finds no walk running starts
    /// it with `read`. A walk that panics yields `T::default()` to its waiters.
    pub(super) fn read(
        &self,
        interest: Option<BTreeSet<u32>>,
        read: impl FnOnce(Option<BTreeSet<u32>>) -> T,
    ) -> T {
        let mut queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
        let id = queue.next;
        match (&mut queue.wanted, interest) {
            (Some(wanted), Some(interest)) => wanted.extend(interest),
            (wanted, _) => *wanted = None,
        }
        *queue.waiting.entry(id).or_default() += 1;
        let mut read = Some(read);
        loop {
            if let Some(result) = queue.results.get(&id).cloned() {
                let waiting = queue.waiting.entry(id).or_default();
                *waiting -= 1;
                if *waiting == 0 {
                    queue.waiting.remove(&id);
                    queue.results.remove(&id);
                }
                return result;
            }
            if !queue.reading && queue.next == id {
                queue.reading = true;
                queue.next += 1;
                let wanted = queue.wanted.replace(BTreeSet::new());
                drop(queue);
                let mut finish = Finish {
                    reads: self,
                    id,
                    result: None,
                };
                let walk = read.take().expect("a caller starts at most one walk");
                finish.result = Some(walk(wanted));
                drop(finish);
                queue = self.queue.lock().unwrap_or_else(|p| p.into_inner());
                continue;
            }
            queue = self.done.wait(queue).unwrap_or_else(|p| p.into_inner());
        }
    }
}

/// Publishes a walk's result, or the default when the walk unwinds, so
/// waiters never wait for a walk that will not finish.
struct Finish<'a, T: Default> {
    reads: &'a SharedReads<T>,
    id: u64,
    result: Option<T>,
}

impl<T: Default> Drop for Finish<'_, T> {
    fn drop(&mut self) {
        let mut queue = self.reads.queue.lock().unwrap_or_else(|p| p.into_inner());
        queue.reading = false;
        match self.result.take() {
            Some(result) => {
                queue.results.insert(self.id, result);
            }
            None => {
                // The walk unwound, and its caller with it: release the
                // caller's place and give everyone else the default.
                let waiting = queue.waiting.entry(self.id).or_default();
                *waiting = waiting.saturating_sub(1);
                if *waiting == 0 {
                    queue.waiting.remove(&self.id);
                } else {
                    queue.results.insert(self.id, T::default());
                }
            }
        }
        drop(queue);
        self.reads.done.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A walk that reports when it starts and waits to be released.
    fn held(
        started: mpsc::Sender<Option<BTreeSet<u32>>>,
        release: Arc<Mutex<mpsc::Receiver<()>>>,
        result: u32,
    ) -> impl FnOnce(Option<BTreeSet<u32>>) -> u32 {
        move |interest| {
            started.send(interest).unwrap();
            release.lock().unwrap().recv().unwrap();
            result
        }
    }

    #[test]
    fn a_retry_after_busy_state_reuses_the_walk_and_a_retry_after_a_prepared_attempt_reads_again() {
        let tracker = Tracker::default();
        let reads = tracker.inject_validation_table_for_test(
            ProcessTable::from_test_members(3, 60_000, Vec::new(), Vec::new()),
            Duration::ZERO,
        );
        let mut receipt = Receipt::default();
        let prepare = |receipt: &mut Receipt| {
            tracker.prepare(
                "busy",
                1,
                std::process::id(),
                crate::subtree_resources::Target::Unavailable("no_container"),
                Declaration::default(),
                receipt,
            )
        };
        {
            let _busy = tracker.state.lock().unwrap();
            assert!(prepare(&mut receipt).is_none());
        }
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert!(prepare(&mut receipt).is_some());
        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "the attempt after busy state reused the first walk"
        );
        assert!(prepare(&mut receipt).is_some());
        assert_eq!(
            reads.load(Ordering::SeqCst),
            2,
            "an attempt after a prepared one, as after a commit conflict, walks again"
        );
        assert_eq!(receipt.finish_until, Some(3));
    }

    #[test]
    fn arrivals_during_a_walk_share_the_next_walk_and_never_the_running_one() {
        let reads = Arc::new(SharedReads::<u32>::default());
        let (started, starts) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let wait = Arc::new(Mutex::new(wait));
        let first = {
            let reads = Arc::clone(&reads);
            let walk = held(started.clone(), Arc::clone(&wait), 1);
            std::thread::spawn(move || reads.read(Some(BTreeSet::from([1])), walk))
        };
        assert_eq!(starts.recv().unwrap(), Some(BTreeSet::from([1])));
        // Three callers arrive while the first walk runs.
        let late: Vec<_> = [2, 3, 4]
            .into_iter()
            .map(|pid| {
                let reads = Arc::clone(&reads);
                let walk = held(started.clone(), Arc::clone(&wait), 2);
                std::thread::spawn(move || reads.read(Some(BTreeSet::from([pid])), walk))
            })
            .collect();
        while reads.queue.lock().unwrap().waiting.get(&1).copied() != Some(3) {
            std::thread::yield_now();
        }
        release.send(()).unwrap();
        assert_eq!(first.join().unwrap(), 1);
        assert_eq!(
            starts.recv().unwrap(),
            Some(BTreeSet::from([2, 3, 4])),
            "one walk reads the union of every waiting caller's interest"
        );
        release.send(()).unwrap();
        for caller in late {
            assert_eq!(
                caller.join().unwrap(),
                2,
                "no caller used the walk that began before it arrived"
            );
        }
        assert!(
            starts.try_recv().is_err(),
            "the waiting callers started only one more walk"
        );
        let queue = reads.queue.lock().unwrap();
        assert!(queue.results.is_empty() && queue.waiting.is_empty());
    }

    #[test]
    fn a_full_walk_request_widens_the_shared_walk_and_a_panic_releases_waiters() {
        let reads = Arc::new(SharedReads::<Option<u32>>::default());
        let (started, starts) = mpsc::channel();
        let (release, wait) = mpsc::channel::<()>();
        let wait = Arc::new(Mutex::new(wait));
        let first = {
            let reads = Arc::clone(&reads);
            let started = started.clone();
            let wait = Arc::clone(&wait);
            std::thread::spawn(move || {
                reads.read(Some(BTreeSet::from([1])), move |interest| {
                    started.send(interest).unwrap();
                    wait.lock().unwrap().recv().unwrap();
                    Some(1)
                })
            })
        };
        starts.recv().unwrap();
        let callers: Vec<_> = [Some(BTreeSet::from([2])), None]
            .into_iter()
            .map(|interest| {
                let reads = Arc::clone(&reads);
                let started = started.clone();
                std::thread::spawn(move || {
                    reads.read(interest, move |interest| {
                        started.send(interest).unwrap();
                        panic!("walk failed");
                    })
                })
            })
            .collect();
        while reads.queue.lock().unwrap().waiting.get(&1).copied() != Some(2) {
            std::thread::yield_now();
        }
        release.send(()).unwrap();
        assert_eq!(first.join().unwrap(), Some(1));
        assert_eq!(starts.recv().unwrap(), None, "a full walk covers everyone");
        let outcomes: Vec<_> = callers.into_iter().map(|caller| caller.join()).collect();
        assert_eq!(
            outcomes.iter().filter(|outcome| outcome.is_err()).count(),
            1,
            "only the caller whose walk panicked unwinds"
        );
        assert!(outcomes
            .into_iter()
            .flatten()
            .all(|result| result.is_none()));
        let queue = reads.queue.lock().unwrap();
        assert!(queue.results.is_empty() && queue.waiting.is_empty() && !queue.reading);
    }
}
