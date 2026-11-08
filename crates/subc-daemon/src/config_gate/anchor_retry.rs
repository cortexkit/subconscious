//! Retry scheduling for an already-committed, non-interactive store anchor.

use std::time::Duration;
use tokio::time::Instant;

pub(super) const ANCHOR_RETRY_INITIAL: Duration = Duration::from_secs(1);
pub(super) const ANCHOR_RETRY_MAX: Duration = Duration::from_secs(60);

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Event {
    Pending,
    Failed(String),
    Recovered,
}

#[derive(Default)]
pub(super) struct AnchorRetry {
    pending: bool,
    next: Option<Instant>,
    delay: Duration,
    last_error: Option<String>,
}

impl AnchorRetry {
    pub(super) fn next(&self) -> Option<Instant> {
        self.next
    }

    fn observe(&mut self, pending: bool, now: Instant, events: &mut Vec<Event>) {
        if pending == self.pending {
            return;
        }
        self.pending = pending;
        if pending {
            self.delay = ANCHOR_RETRY_INITIAL;
            self.next = Some(now + self.delay);
            events.push(Event::Pending);
        } else {
            self.next = None;
            self.delay = Duration::ZERO;
            self.last_error = None;
            events.push(Event::Recovered);
        }
    }

    /// The closure retries the retained backend, never opens a keychain or
    /// invokes an interactive provider. It returns the post-attempt state.
    pub(super) fn poll(
        &mut self,
        pending: bool,
        now: Instant,
        operator_command: bool,
        retry: impl FnOnce() -> (bool, Option<String>),
    ) -> Vec<Event> {
        let mut events = vec![];
        self.observe(pending, now, &mut events);
        if !pending || (!operator_command && !self.next.is_some_and(|next| now >= next)) {
            return events;
        }
        let (pending, error) = retry();
        if let Some(error) = error {
            if self.last_error.as_ref() != Some(&error) {
                events.push(Event::Failed(error.clone()));
                self.last_error = Some(error);
            }
        }
        self.observe(pending, now, &mut events);
        if pending && !operator_command {
            self.delay = (self.delay * 2).min(ANCHOR_RETRY_MAX);
            self.next = Some(now + self.delay);
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn config_anchor_retry_backoff_edges_cap_and_changed_reason_logging() {
        let start = Instant::now();
        let mut schedule = AnchorRetry::default();
        let mut calls = 0;
        let mut events = schedule.poll(true, start, false, || panic!("must wait one second"));
        for (index, seconds) in [1, 3, 7, 15, 31, 63, 123, 183].into_iter().enumerate() {
            let at = start + Duration::from_secs(seconds);
            assert_eq!(schedule.next(), Some(at));
            tokio::time::advance(at - Instant::now() - Duration::from_nanos(1)).await;
            assert!(schedule
                .poll(true, Instant::now(), false, || panic!(
                    "retry fired before deadline"
                ))
                .is_empty());
            tokio::time::advance(Duration::from_nanos(1)).await;
            events.extend(schedule.poll(true, Instant::now(), false, || {
                calls += 1;
                (
                    true,
                    Some(if index < 3 { "locked" } else { "denied" }.into()),
                )
            }));
        }
        assert_eq!(calls, 8);
        assert_eq!(
            events,
            vec![
                Event::Pending,
                Event::Failed("locked".into()),
                Event::Failed("denied".into())
            ]
        );
        assert_eq!(schedule.next(), Some(start + Duration::from_secs(243)));
    }

    #[tokio::test(start_paused = true)]
    async fn config_anchor_retry_recovers_resets_and_never_calls_clear_backend() {
        let mut schedule = AnchorRetry::default();
        assert!(schedule
            .poll(false, Instant::now(), false, || panic!("no pending anchor"))
            .is_empty());
        assert!(schedule
            .poll(false, Instant::now(), true, || panic!(
                "command must not retry a clear anchor"
            ))
            .is_empty());
        let mut backend_fails = true;
        let mut calls = 0;
        assert_eq!(
            schedule.poll(true, Instant::now(), false, || panic!("not due")),
            vec![Event::Pending]
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            schedule.poll(true, Instant::now(), false, || {
                calls += 1;
                (backend_fails, Some("temporary".into()))
            }),
            vec![Event::Failed("temporary".into())]
        );
        backend_fails = false;
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(
            schedule.poll(true, Instant::now(), false, || {
                calls += 1;
                (backend_fails, None)
            }),
            vec![Event::Recovered]
        );
        assert_eq!(calls, 2);
        assert!(schedule.next().is_none());
        tokio::time::advance(Duration::from_secs(600)).await;
        assert!(schedule
            .poll(false, Instant::now(), false, || panic!(
                "recovered backend was called"
            ))
            .is_empty());
        assert_eq!(
            schedule.poll(true, Instant::now(), false, || panic!(
                "new pending waits one second"
            )),
            vec![Event::Pending]
        );
        assert_eq!(
            schedule.next(),
            Some(Instant::now() + Duration::from_secs(1))
        );
        assert_eq!(
            schedule.poll(true, Instant::now(), true, || (false, None)),
            vec![Event::Recovered]
        );
        assert!(schedule.next().is_none());
    }
}
