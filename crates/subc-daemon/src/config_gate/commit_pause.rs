//! Exclude only an explicitly held test fence from a declaration's deadline.

use std::cell::RefCell;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

thread_local! {
    static CURRENT: RefCell<Option<Clock>> = const { RefCell::new(None) };
}

#[derive(Clone, Default)]
struct State {
    held_since: Option<Instant>,
    elapsed: Duration,
}

#[derive(Clone)]
pub(crate) struct Clock(watch::Sender<State>);

impl Clock {
    pub(crate) fn new() -> Self {
        Self(watch::channel(State::default()).0)
    }

    pub(crate) fn install(clock: Option<Self>) -> Installed {
        CURRENT.with(|current| *current.borrow_mut() = clock);
        Installed
    }

    pub(crate) async fn timeout<F: std::future::Future>(
        &self,
        deadline: Instant,
        future: F,
    ) -> Result<F::Output, tokio::time::error::Elapsed> {
        let mut changes = self.0.subscribe();
        tokio::pin!(future);
        loop {
            let state = changes.borrow_and_update().clone();
            if state.held_since.is_some() {
                tokio::select! {
                    result = &mut future => return Ok(result),
                    _ = changes.changed() => {},
                }
            } else {
                tokio::select! {
                    biased;
                    _ = changes.changed() => {},
                    result = tokio::time::timeout_at(deadline + state.elapsed, &mut future) => {
                        // The blocking worker can enter the hold concurrently
                        // with the timer firing. Recheck before expiring it.
                        let current = changes.borrow();
                        if result.is_ok() || (current.held_since.is_none()
                            && Instant::now() >= deadline + current.elapsed)
                        {
                            return result;
                        }
                    },
                }
            }
        }
    }
}

pub(crate) struct Installed;

impl Drop for Installed {
    fn drop(&mut self) {
        CURRENT.with(|current| *current.borrow_mut() = None);
    }
}

pub(crate) struct Hold(Option<Clock>);

impl Hold {
    pub(crate) fn current() -> Self {
        let clock = CURRENT.with(|current| current.borrow().clone());
        if let Some(clock) = &clock {
            clock
                .0
                .send_modify(|state| state.held_since = Some(Instant::now()));
        }
        Self(clock)
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        if let Some(clock) = &self.0 {
            clock.0.send_modify(|state| {
                if let Some(started) = state.held_since.take() {
                    state.elapsed += started.elapsed();
                }
            });
        }
    }
}

#[tokio::test(start_paused = true)]
async fn armed_clock_does_not_suspend_work_outside_the_hold() {
    let clock = Clock::new();
    let _installed = Clock::install(Some(clock.clone()));
    let hold = Hold::current();
    let start = Instant::now();
    let waiter = tokio::spawn(async move {
        clock
            .timeout(
                start + Duration::from_millis(200),
                std::future::pending::<()>(),
            )
            .await
    });
    tokio::time::advance(Duration::from_secs(3)).await;
    assert!(!waiter.is_finished(), "the synthetic fence is still held");
    drop(hold);
    tokio::time::advance(Duration::from_millis(199)).await;
    assert!(
        !waiter.is_finished(),
        "the hold must not spend validation time"
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(waiter.await.unwrap().is_err());
}
