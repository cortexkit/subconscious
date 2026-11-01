//! One-shot authorization on the system bus; only our own reply grants anything.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use rustix::process::{geteuid, getpid, pidfd_open, PidfdFlags};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{Fd, Value};

use crate::{polkit, Outcome, Withdraw};

const AUTHORITY: &str = "org.freedesktop.PolicyKit1";
const AUTHORITY_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";
const AUTHORITY_INTERFACE: &str = "org.freedesktop.PolicyKit1.Authority";
type Subject = (&'static str, HashMap<&'static str, Value<'static>>);
type Reply = ((bool, bool, HashMap<String, String>),);
const WITHDRAW_BUDGET: Duration = Duration::from_secs(5);
const WITHDRAW_RETRY_INTERVAL: Duration = Duration::from_millis(100);

fn authority(connection: &Connection) -> zbus::Result<Proxy<'_>> {
    Proxy::new(connection, AUTHORITY, AUTHORITY_PATH, AUTHORITY_INTERFACE)
}

#[derive(Default)]
struct State {
    withdrawn: bool,
    finished: bool,
    interactive: bool,
}

struct Pending {
    connection: Connection,
    id: String,
    state: Mutex<State>,
    finished: Condvar,
}

impl Withdraw for Pending {
    fn withdraw(&self) {
        withdraw_until(
            &self.state,
            &self.finished,
            WITHDRAW_BUDGET,
            |interactive, deadline| {
                let id = if interactive {
                    self.id.clone()
                } else {
                    format!("{}-probe", self.id)
                };
                // Use the checking connection, but deadline-bound the underlying
                // async cancellation call so an unresponsive authority cannot trap
                // this blocking worker. The prompt itself still awaits its reply.
                let _ = poll_until(
                    self.connection.inner().call_method(
                        Some(AUTHORITY),
                        AUTHORITY_PATH,
                        Some(AUTHORITY_INTERFACE),
                        "CancelCheckAuthorization",
                        &(id,),
                    ),
                    deadline,
                );
            },
        );
    }
}

struct WakeThread(std::thread::Thread);

impl Wake for WakeThread {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Unlike the authorization check, cancellation must not wait forever for a
/// method reply. Dropping its future at the deadline leaves the checking
/// connection and the independently waiting authorization call alive.
fn poll_until<F: Future>(future: F, deadline: Instant) -> Option<F::Output> {
    let waker = Waker::from(Arc::new(WakeThread(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        match future.as_mut().poll(&mut context) {
            Poll::Ready(result) => return Some(result),
            Poll::Pending => std::thread::park_timeout(remaining),
        }
    }
}

fn withdraw_until(
    state: &Mutex<State>,
    finished: &Condvar,
    budget: Duration,
    mut cancel: impl FnMut(bool, Instant),
) {
    let deadline = Instant::now() + budget;
    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
    guard.withdrawn = true;
    while !guard.finished && Instant::now() < deadline {
        let interactive = guard.interactive;
        drop(guard);
        // Cancellation may beat the initial method dispatch. Retry only within
        // the shared budget; withdrawn remains set even if the authority hangs.
        cancel(interactive, deadline);
        guard = state.lock().unwrap_or_else(|e| e.into_inner());
        if !guard.finished {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            guard = finished
                .wait_timeout(guard, remaining.min(WITHDRAW_RETRY_INTERVAL))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

/// polkit's systemd session monitor resolves a session-less user service via
/// sd_pid_get_owner_uid -> sd_uid_get_display, then finds the session's agent.
/// See polkit 126, polkitbackendsessionmonitor-systemd.c:419-428 and
/// polkitbackendinteractiveauthority.c:1733-1777. Keep the process identity
/// pinned with a pidfd instead of trusting a caller-supplied session id.
fn subject(uid: u32) -> Option<Subject> {
    let mut values = HashMap::from([("uid", Value::from(i32::try_from(uid).ok()?))]);
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let start: u64 = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()?;
    // polkit 122 ignores pidfd; newer authorities prefer it when present
    // (polkit 126, polkitsubject.c:435-469). Carry both representations.
    values.insert("pid", Value::from(std::process::id()));
    values.insert("start-time", Value::from(start));
    if let Ok(fd) = pidfd_open(getpid(), PidfdFlags::empty()) {
        values.insert("pidfd", Value::from(Fd::from(fd)));
    }
    Some(("unix-process", values))
}

pub(crate) fn prompt(text: &str, publish: Box<dyn FnOnce(Arc<dyn Withdraw>) + Send>) -> Outcome {
    prompt_as_user(geteuid().as_raw(), text, publish)
}

fn prompt_as_user(
    uid: u32,
    text: &str,
    publish: Box<dyn FnOnce(Arc<dyn Withdraw>) + Send>,
) -> Outcome {
    // polkit 126, polkitbackendinteractiveauthority.c:1135-1139 grants root
    // subjects unconditionally, without authentication. Never treat that
    // shortcut as operator presence, even when an action file is installed.
    if uid == 0 {
        return Outcome::ProviderErrorWithReason {
            reason: "root_user",
        };
    }
    if text.is_empty() || text.contains('\0') {
        return Outcome::ProviderError;
    }
    if !std::path::Path::new(&polkit::action_path(uid)).exists() {
        return Outcome::SetupRequired;
    }
    let Ok(connection) = Connection::system() else {
        return Outcome::ProviderError;
    };
    let Some(subject) = subject(uid) else {
        return Outcome::ProviderError;
    };
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let pending = Arc::new(Pending {
        connection,
        id: format!(
            "subc-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ),
        state: Mutex::new(State::default()),
        finished: Condvar::new(),
    });
    publish(pending.clone());
    let result = if pending
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .withdrawn
    {
        Outcome::Declined
    } else {
        match authority(&pending.connection).and_then(|proxy| {
            let ((authorized, challenge, _),): Reply = proxy.call(
                "CheckAuthorization",
                &(
                    &subject,
                    polkit::action_id(uid),
                    polkit::details(text),
                    0u32,
                    format!("{}-probe", pending.id),
                ),
            )?;
            // A policy denial without a challenge asked nobody. Do not report
            // it as a person's refusal (e.g. a session-less headless service).
            if let Some(outcome) = polkit::preflight_outcome(authorized, challenge) {
                return Ok(outcome);
            }
            {
                let mut state = pending.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.withdrawn {
                    return Ok(Outcome::Declined);
                }
                state.interactive = true;
            }
            let ((authorized, challenge, details),): Reply = proxy.call(
                "CheckAuthorization",
                &(
                    &subject,
                    polkit::action_id(uid),
                    polkit::details(text),
                    1u32,
                    &pending.id,
                ),
            )?;
            Ok(if details.contains_key("polkit.dismissed") {
                Outcome::Declined
            } else {
                polkit::outcome(authorized, challenge)
            })
        }) {
            Ok(outcome) => outcome,
            Err(_) => Outcome::ProviderError,
        }
    };
    let mut state = pending.state.lock().unwrap_or_else(|e| e.into_inner());
    state.finished = true;
    pending.finished.notify_all();
    if state.withdrawn {
        Outcome::Declined
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_user_is_provider_error_before_setup_or_bus() {
        assert_eq!(
            prompt_as_user(
                0,
                "do not authenticate root",
                Box::new(|_| panic!("root must never publish a prompt"))
            ),
            Outcome::ProviderErrorWithReason {
                reason: "root_user"
            }
        );
    }

    #[test]
    fn withdraw_bounds_a_hung_cancel_without_finishing_the_prompt() {
        assert_eq!(WITHDRAW_BUDGET, Duration::from_secs(5));
        assert_eq!(WITHDRAW_RETRY_INTERVAL, Duration::from_millis(100));
        let state = Mutex::new(State::default());
        let finished = Condvar::new();
        let start = Instant::now();
        let mut attempts = 0;
        withdraw_until(
            &state,
            &finished,
            Duration::from_millis(20),
            |interactive, deadline| {
                assert!(!interactive);
                attempts += 1;
                assert!(poll_until(std::future::pending::<()>(), deadline).is_none());
            },
        );
        assert_eq!(attempts, 1);
        assert!(start.elapsed() < Duration::from_secs(1));
        let state = state.lock().unwrap();
        assert!(state.withdrawn);
        assert!(
            !state.finished,
            "only the actual authorization reply ends the prompt"
        );
    }

    #[test]
    fn completed_prompt_needs_no_cancel() {
        let state = Mutex::new(State {
            finished: true,
            ..State::default()
        });
        withdraw_until(&state, &Condvar::new(), WITHDRAW_BUDGET, |_, _| {
            panic!("completed prompt must not cancel")
        });
        assert!(state.lock().unwrap().withdrawn);
    }

    #[test]
    fn cancellation_reply_wakes_the_deadline_bounded_worker() {
        let mut polled = false;
        let reply = std::future::poll_fn(|context| {
            if polled {
                Poll::Ready(7)
            } else {
                polled = true;
                context.waker().wake_by_ref();
                Poll::Pending
            }
        });
        assert_eq!(
            poll_until(reply, Instant::now() + Duration::from_secs(1)),
            Some(7)
        );
    }

    fn assert_wire_signature(message: &zbus::Message, expected: &str) {
        // The parsed signature normalizes single-argument structs. Check the
        // encoded header to retain the distinction between bb... and (bb...).
        let mut encoded = vec![u8::try_from(expected.len()).unwrap()];
        encoded.extend_from_slice(expected.as_bytes());
        encoded.push(0);
        assert!(message
            .data()
            .bytes()
            .windows(encoded.len())
            .any(|bytes| bytes == encoded));
    }

    #[test]
    fn process_subject_uses_wire_types_and_keeps_legacy_fallback() {
        let (kind, values) = subject(geteuid().as_raw()).unwrap();
        assert_eq!(kind, "unix-process");
        assert_eq!(values["uid"].value_signature().to_string(), "i");
        assert_eq!(values["pid"].value_signature().to_string(), "u");
        assert_eq!(values["start-time"].value_signature().to_string(), "t");
        if let Some(fd) = values.get("pidfd") {
            assert_eq!(fd.value_signature().to_string(), "h");
        }
    }

    #[test]
    fn authorization_wire_signature_and_struct_reply_match_polkit() {
        let subject = subject(geteuid().as_raw()).unwrap();
        let call = zbus::Message::method_call(AUTHORITY_PATH, "CheckAuthorization")
            .unwrap()
            .build(&(
                subject,
                polkit::action_id(1234),
                polkit::details("literal $(x)"),
                1u32,
                "test-id",
            ))
            .unwrap();
        assert_wire_signature(&call, "(sa{sv})sa{ss}us");
        let reply = zbus::Message::method_return(&call.header())
            .unwrap()
            .build(&((true, false, HashMap::<String, String>::new()),))
            .unwrap();
        assert_wire_signature(&reply, "(bba{ss})");
        let ((authorized, challenge, details),): Reply = reply.body().deserialize().unwrap();
        assert!(authorized);
        assert!(!challenge);
        assert!(details.is_empty());
    }
}
