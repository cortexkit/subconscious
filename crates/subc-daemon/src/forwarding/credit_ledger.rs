use std::collections::HashMap;

use super::{
    stalled_cancels::{CancelObservation, StalledCancels},
    RouteBinding,
};

#[derive(Debug)]
pub(super) struct RequestCredit {
    subscription: bool,
    pub(super) watch_credit: bool,
    excluded_from_drain: bool,
    cancel: Option<CancelObservation>,
}

/// Held credits by correlation, with exact totals kept beside them so a busy
/// refusal can report the occupancy without walking every held credit.
#[derive(Debug, Default)]
pub(super) struct CreditLedger {
    by_corr: HashMap<u64, Vec<RequestCredit>>,
    total: usize,
    pub(super) subscriptions: usize,
    cancel_tracking_closed: bool,
}

impl CreditLedger {
    pub(super) fn acquire(&mut self, corr: u64, subscription: bool, watch_credit: bool) {
        self.total += 1;
        self.subscriptions += usize::from(subscription);
        self.by_corr.entry(corr).or_default().push(RequestCredit {
            subscription,
            watch_credit,
            excluded_from_drain: false,
            cancel: None,
        });
    }

    pub(super) fn release(&mut self, corr: u64) -> Option<RequestCredit> {
        let credits = self.by_corr.get_mut(&corr)?;
        let released = credits.pop();
        if credits.is_empty() {
            self.by_corr.remove(&corr);
        }
        if let Some(credit) = &released {
            self.total -= 1;
            self.subscriptions -= usize::from(credit.subscription);
        }
        released
    }

    pub(super) fn record_cancel(
        &mut self,
        route: &RouteBinding,
        corr: u64,
        monitor: &StalledCancels,
    ) {
        if self.cancel_tracking_closed {
            return;
        }
        // Match the terminal's lookup. A repeated CANCEL must not restart the
        // grace period or rearm the warning for the same request.
        if let Some(credit) = self
            .by_corr
            .get_mut(&corr)
            .and_then(|credits| credits.last_mut())
        {
            credit
                .cancel
                .get_or_insert_with(|| monitor.record(route, corr, credit.subscription));
        }
    }

    pub(super) fn close_cancel_tracking(&mut self) {
        self.cancel_tracking_closed = true;
        for credit in self.by_corr.values_mut().flatten() {
            credit.cancel = None;
        }
    }

    pub(super) fn capture_subscription_exclusions(&mut self) -> u32 {
        let mut excluded = 0u32;
        for credit in self.by_corr.values_mut().flatten() {
            if credit.subscription && !credit.excluded_from_drain {
                credit.excluded_from_drain = true;
                excluded = excluded.saturating_add(1);
            }
        }
        excluded
    }

    pub(super) fn in_flight(&self) -> usize {
        self.total
    }

    pub(super) fn drain_in_flight(&self) -> usize {
        self.by_corr
            .values()
            .flatten()
            .filter(|credit| !credit.excluded_from_drain)
            .count()
    }

    /// The correlation ids of the requests a drain still waits on, one entry
    /// per held credit, in ascending order.
    pub(super) fn drain_held_corrs(&self) -> Vec<u64> {
        let mut corrs = self
            .by_corr
            .iter()
            .flat_map(|(corr, credits)| {
                credits
                    .iter()
                    .filter(|credit| !credit.excluded_from_drain)
                    .map(move |_| *corr)
            })
            .collect::<Vec<_>>();
        corrs.sort_unstable();
        corrs
    }
}
