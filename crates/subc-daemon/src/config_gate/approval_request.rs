use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
    Arc, Weak,
};

use super::{GateError, Result, RuntimeOwner};
use crate::ConnectionId;

const LIVE: u8 = 0;
const CANCELLED: u8 = 1;
const PUBLISHING: u8 = 2;

/// Reader reservation shared with the blocking begin job and publication claim.
/// Cancellation never needs the configuration owner or a disk/keychain check.
#[derive(Default)]
pub(crate) struct ApprovalRequest {
    phase: AtomicU8,
    claimed: AtomicBool,
    attempt: AtomicU64,
    late_reported: AtomicBool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CancelApprovalRequest {
    Unknown,
    Withdrawn,
    TooLate,
    AlreadyReported,
}

impl ApprovalRequest {
    pub(super) fn for_attempt(attempt: u64) -> Arc<Self> {
        Arc::new(Self {
            attempt: AtomicU64::new(attempt),
            ..Self::default()
        })
    }
    pub(crate) fn ensure_live(&self) -> Result<()> {
        if self.phase.load(Ordering::Acquire) == LIVE {
            Ok(())
        } else {
            Err(GateError::new(
                "approval_surface_unavailable",
                "approval caller withdrew the attempt",
            ))
        }
    }

    pub(crate) fn claim_publication(&self) -> Result<()> {
        self.phase
            .compare_exchange(LIVE, PUBLISHING, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| {
                GateError::new(
                    "approval_surface_unavailable",
                    "approval caller withdrew before publication",
                )
            })
    }

    pub(super) fn cancel(&self) -> CancelApprovalRequest {
        match self
            .phase
            .compare_exchange(LIVE, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
        {
            Err(PUBLISHING) => CancelApprovalRequest::TooLate,
            _ => CancelApprovalRequest::Withdrawn,
        }
    }

    pub(super) fn attempt(&self) -> u64 {
        self.attempt.load(Ordering::Acquire)
    }
}

pub(crate) struct ApprovalRequestGuard {
    owner: Weak<RuntimeOwner>,
    caller: ConnectionId,
    correlation: u64,
    pub(crate) request: Arc<ApprovalRequest>,
}

impl Drop for ApprovalRequestGuard {
    fn drop(&mut self) {
        self.request.cancel();
        if let Some(owner) = self.owner.upgrade() {
            if let Ok(mut index) = owner.approval_request_index.lock() {
                let key = (self.caller, self.correlation);
                if index
                    .get(&key)
                    .is_some_and(|request| Arc::ptr_eq(request, &self.request))
                {
                    index.remove(&key);
                }
            }
            let attempt = self.request.attempt();
            if attempt != 0 {
                owner.cancel_approval(attempt);
            }
        }
    }
}

impl RuntimeOwner {
    pub(crate) fn reserve_approval_request(
        self: &Arc<Self>,
        caller: ConnectionId,
        correlation: u64,
    ) -> Option<ApprovalRequestGuard> {
        let mut index = self.approval_request_index.lock().ok()?;
        let std::collections::hash_map::Entry::Vacant(entry) = index.entry((caller, correlation))
        else {
            return None;
        };
        let request = entry.insert(Arc::default()).clone();
        Some(ApprovalRequestGuard {
            owner: Arc::downgrade(self),
            caller,
            correlation,
            request,
        })
    }

    pub(crate) fn claim_approval_request(
        self: &Arc<Self>,
        caller: ConnectionId,
        correlation: u64,
    ) -> Result<ApprovalRequestGuard> {
        let request = self
            .approval_request_index
            .lock()
            .map_err(|_| {
                GateError::new(
                    "config_unavailable",
                    "approval cancellation index is poisoned",
                )
            })?
            .entry((caller, correlation))
            .or_default()
            .clone();
        if request.claimed.swap(true, Ordering::AcqRel) {
            return Err(GateError::new(
                "config_busy",
                "approval request correlation is already pending",
            ));
        }
        let guard = ApprovalRequestGuard {
            owner: Arc::downgrade(self),
            caller,
            correlation,
            request,
        };
        guard.request.ensure_live()?;
        Ok(guard)
    }

    pub(crate) fn track_reserved_approval_request(
        &self,
        attempt: u64,
        caller: ConnectionId,
        correlation: u64,
        request: &Arc<ApprovalRequest>,
    ) -> Result<()> {
        let mut state = self.lock()?;
        state.gate.approval_digest(attempt)?;
        // Publish the slot identity before checking cancellation, so either this
        // job cleans it up or the reader/drop callback can cancel the same slot.
        request.attempt.store(attempt, Ordering::Release);
        if let Err(error) = request.ensure_live() {
            state.cancel_approval(attempt);
            return Err(error);
        }
        state
            .approval_requests
            .insert((caller, correlation), attempt);
        Ok(())
    }

    pub(crate) fn cancel_approval_request_outcome(
        &self,
        caller: ConnectionId,
        correlation: u64,
    ) -> CancelApprovalRequest {
        let request = self
            .approval_request_index
            .lock()
            .ok()
            .and_then(|index| index.get(&(caller, correlation)).cloned());
        let Some(request) = request else {
            return CancelApprovalRequest::Unknown;
        };
        let outcome = request.cancel();
        if outcome == CancelApprovalRequest::TooLate
            && request.late_reported.swap(true, Ordering::AcqRel)
        {
            return CancelApprovalRequest::AlreadyReported;
        }
        if outcome == CancelApprovalRequest::Withdrawn && request.attempt() != 0 {
            self.cancel_approval(request.attempt());
        }
        outcome
    }
}
