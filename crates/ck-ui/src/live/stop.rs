//! Stop uses a separate control connection so it can wait for the child to
//! exit without lengthening the deadline for the app's regular status polls.

use std::future::Future;

use super::{connect, control_error, now, Arc, CallError, Duration, Shared, State};
use crate::model::{ActionKind, ActionPhase, ActionResult, ModuleAction, Status};

fn response_budget(drain_ms: Option<u64>) -> Duration {
    // supervisor.list reports the module's effective drain_timeout_ms. The
    // daemon can spend it once waiting for in-flight requests and again waiting
    // for the child to exit before killing it. Older daemons omit this field;
    // use the daemon's 30 s DEFAULT_DRAIN_TIMEOUT. As in `ck module stop`, the
    // 2 s margin allows for collecting the exit status and receiving the reply.
    // The daemon does not cap the post-kill wait.
    Duration::from_millis(drain_ms.unwrap_or(30_000))
        .saturating_mul(2)
        .saturating_add(Duration::from_secs(2))
}

pub(super) fn request(shared: &Arc<Shared>, module: String) {
    let Some(budget) = begin(shared, &module) else {
        return;
    };
    let shared = Arc::clone(shared);
    tokio::spawn(async move {
        follow(&shared, &module, budget, send(&shared, &module, budget)).await;
    });
}

fn begin(shared: &Shared, module: &str) -> Option<Duration> {
    shared.update(|s| {
        if let Some(action) = s.actions.get(module) {
            if action.kind != ActionKind::Stop {
                record_result(
                    s,
                    module,
                    Err(StopFailure::Refused("another action is in progress".into())),
                );
            }
            return None;
        }
        s.actions.insert(
            module.into(),
            ModuleAction {
                kind: ActionKind::Stop,
                phase: ActionPhase::Asking,
                started_at: now(),
            },
        );
        let drain_ms = s
            .roster
            .value
            .iter()
            .flatten()
            .find(|entry| entry.module_id == module)
            .and_then(|entry| entry.drain_timeout_ms);
        Some(response_budget(drain_ms))
    })
}

#[derive(Debug)]
enum StopFailure {
    Refused(String),
    Unconfirmed,
}

async fn send(shared: &Shared, module: &str, budget: Duration) -> Result<(), StopFailure> {
    if shared.conn().is_none() {
        return Err(StopFailure::Refused("the daemon isn't connected".into()));
    }
    let (consumer, _) = connect(shared.connection_file.as_deref(), budget)
        .await
        .map_err(StopFailure::Refused)?;
    consumer
        .supervisor_set_enabled(module, false)
        .await
        .map_err(|error| match error {
            CallError::OutcomeUnknown(_) => StopFailure::Unconfirmed,
            _ => StopFailure::Refused(control_error(&error)),
        })?;
    Ok(())
}

async fn follow(
    shared: &Shared,
    module: &str,
    budget: Duration,
    send: impl Future<Output = Result<(), StopFailure>>,
) {
    let result = tokio::time::timeout(budget, async {
        send.await?;
        // The reply follows exit. Keep showing "stopping" until the module
        // list also says stopped, so an old running row cannot reappear.
        let mut snapshots = shared.tx.subscribe();
        loop {
            if snapshots
                .borrow_and_update()
                .modules
                .data
                .iter()
                .any(|m| m.id == module && m.status == Status::Stopped)
            {
                return Ok(());
            }
            if snapshots.changed().await.is_err() {
                return Err(StopFailure::Unconfirmed);
            }
        }
    })
    .await
    .unwrap_or(Err(StopFailure::Unconfirmed));
    shared.update(|s| {
        s.actions.remove(module);
        record_result(s, module, result);
    });
}

fn record_result(s: &mut State, module: &str, result: Result<(), StopFailure>) {
    let completion = s.stop_completions.entry(module.into()).or_default();
    completion.sequence += 1;
    completion.unconfirmed = matches!(result, Err(StopFailure::Unconfirmed));
    let ok = result.is_ok();
    let text = match result {
        Ok(()) => format!("{module} stopped"),
        Err(StopFailure::Refused(why)) => format!("{module}: stop refused: {why}"),
        Err(StopFailure::Unconfirmed) => format!("{module}: stop not confirmed"),
    };
    s.last_result = Some(ActionResult {
        at: now(),
        text,
        ok,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::run::tests::test_shared;
    use subc_control::{SupervisorEntry, SupervisorHealthStatus};
    use tokio::sync::oneshot;
    use tokio::time::sleep;

    const MODULE: &str = "worker";

    fn record(state: &mut State, status: &str, drain_ms: u64) {
        let running = status != "disabled";
        state.record_roster(
            vec![SupervisorEntry::new(
                MODULE,
                status,
                status == "running",
                running,
                SupervisorHealthStatus::Ok,
            )
            .with_drain_timeout_ms(Some(drain_ms))],
            now(),
        );
    }

    #[tokio::test(start_paused = true)]
    async fn app_stop_waits_for_a_twenty_second_ack_and_the_stopped_roster() {
        let mut state = State::empty("This computer");
        record(&mut state, "running", 30_000);
        let shared = test_shared(state);
        let budget = begin(&shared, MODULE).unwrap();
        assert_eq!(budget, Duration::from_secs(62));
        assert!(
            begin(&shared, MODULE).is_none(),
            "a second stop is not sent"
        );

        let (ack, reply) = oneshot::channel();
        tokio::join!(
            follow(&shared, MODULE, budget, async {
                reply.await.unwrap();
                Ok(())
            }),
            async {
                sleep(Duration::from_secs(20)).await;
                assert!(shared.tx.borrow().modules.data[0].action.is_some());
                assert!(shared.tx.borrow().last_result.is_none());
                ack.send(()).unwrap();
                sleep(Duration::from_secs(1)).await;
                assert!(
                    shared.tx.borrow().modules.data[0].action.is_some(),
                    "the roster still says running"
                );
                shared.update(|s| record(s, "disabled", 30_000));
            },
        );

        let snap = shared.tx.borrow();
        assert_eq!(snap.modules.data[0].status, Status::Stopped);
        assert!(snap.modules.data[0].action.is_none());
        assert_eq!(snap.last_result.as_ref().unwrap().text, "worker stopped");
        assert!(snap.last_result.as_ref().unwrap().ok);
        assert_eq!(snap.stop_completions[MODULE].sequence, 1);
        assert!(!snap.stop_completions[MODULE].unconfirmed);
    }

    #[tokio::test(start_paused = true)]
    async fn app_stop_deadline_reports_unconfirmed_without_claiming_exit() {
        let mut state = State::empty("This computer");
        record(&mut state, "running", 3_000);
        let shared = test_shared(state);
        let budget = begin(&shared, MODULE).unwrap();
        assert_eq!(budget, Duration::from_secs(8));
        assert_eq!(response_budget(None), Duration::from_secs(62));
        let started = tokio::time::Instant::now();
        follow(&shared, MODULE, budget, std::future::pending()).await;
        assert_eq!(started.elapsed(), Duration::from_secs(8));

        let snap = shared.tx.borrow();
        assert_eq!(snap.modules.data[0].status, Status::Ok);
        assert!(snap.modules.data[0].action.is_none());
        assert_eq!(
            snap.last_result.as_ref().unwrap().text,
            "worker: stop not confirmed"
        );
        assert!(!snap.last_result.as_ref().unwrap().ok);
        assert_eq!(snap.stop_completions[MODULE].sequence, 1);
        assert!(snap.stop_completions[MODULE].unconfirmed);
    }
}
