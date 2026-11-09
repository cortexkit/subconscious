use std::sync::atomic::{AtomicU8, Ordering};

const OPEN: u8 = 0;
const COMMITTING: u8 = 1;
const CANCELLED: u8 = 2;

/// A timeout and the short accounting mutation compete for one atomic decision.
/// A caller that loses cancellation must await the mutation's actual result;
/// it must never report a timeout for work already allowed to commit.
#[derive(Debug, Default)]
pub(crate) struct CommitFence(AtomicU8);

impl CommitFence {
    pub(crate) fn committed(&self) -> bool {
        self.0.load(Ordering::Acquire) == COMMITTING
    }
    pub(crate) fn cancel(&self) -> bool {
        match self
            .0
            .compare_exchange(OPEN, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) | Err(CANCELLED) => true,
            Err(COMMITTING) => false,
            Err(_) => false,
        }
    }

    /// Call only after every lock and validation needed for commit is held.
    pub(crate) fn claim_commit(&self) -> bool {
        self.0
            .compare_exchange(OPEN, COMMITTING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn timeout_and_commit_have_one_atomic_winner() {
        let cancelled = CommitFence::default();
        assert!(cancelled.cancel());
        assert!(
            !cancelled.claim_commit(),
            "a timed-out request cannot mutate later"
        );
        let committing = CommitFence::default();
        assert!(committing.claim_commit());
        assert!(
            !committing.cancel(),
            "already committing work cannot be reported as timed out"
        );
        assert!(
            !committing.claim_commit(),
            "a second application cannot reuse the fence"
        );
        let fence = Arc::new(CommitFence::default());
        let barrier = Arc::new(Barrier::new(2));
        let worker = {
            let fence = Arc::clone(&fence);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                fence.claim_commit()
            })
        };
        barrier.wait();
        let timed_out = fence.cancel();
        assert_ne!(
            timed_out,
            worker.join().unwrap(),
            "exactly one outcome is legal"
        );
    }
}
