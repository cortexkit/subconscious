//! Carries the Add a phone exchange's calls to callosum and hands the
//! results back to the app.
//!
//! The exchange itself ([`crate::pairing::flow`]) decides what to call; this
//! makes each call and turns its reply into an [`Event`]. Opens and confirms
//! go on callosum's shared management route. Each window's
//! `callosum.pairing_qr_watch` subscription runs on a route opened for that
//! watch alone and closed when the watch ends, so a watch held open for
//! minutes never holds up another call to callosum.
//!
//! Callosum sits behind the [`Callosum`] trait so the tests can script it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::pairing::flow::{
    Command, ConfirmFailure, Event, Opened, Refusal, Verdict, WatchEvent, CLASS,
};
use crate::pairing::Claim;
use crate::time::Timestamp;

/// How long an open may take.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// A call that came back without a reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallFailure {
    /// Callosum, or the daemon in front of it, refused the call.
    Refused(Refusal),
    /// The call timed out or its connection went away; whether callosum
    /// acted on it is unknown.
    Lost,
}

/// The three pairing operations, as ck sends them.
pub trait Callosum: Send + Sync + 'static {
    /// One `{"method", "params"}` request on callosum's shared route.
    fn call(
        &self,
        method: &'static str,
        params: Value,
        timeout: Duration,
    ) -> BoxFuture<'static, Result<Value, CallFailure>>;

    /// Subscribes to a window's `callosum.pairing_qr_watch` on a route of
    /// its own.
    fn watch(
        &self,
        window_id: String,
    ) -> BoxFuture<'static, Result<Box<dyn WatchStream>, CallFailure>>;
}

/// One window's watch subscription.
pub trait WatchStream: Send {
    /// The next event, or `None` once the stream has ended for any reason.
    fn next(&mut self) -> BoxFuture<'_, Option<Value>>;
    /// Ends the subscription and closes its route.
    fn close(self: Box<Self>) -> BoxFuture<'static, ()>;
}

/// Makes the exchange's calls. Results go to `sink`, the clock is `now`.
pub struct Pairing {
    callosum: Arc<dyn Callosum>,
    sink: Arc<dyn Fn(Event) + Send + Sync>,
    now: fn() -> Timestamp,
    /// The watches running, by window, each with the switch that stops it.
    watches: Mutex<HashMap<String, oneshot::Sender<()>>>,
}

impl Pairing {
    pub fn new(
        callosum: Arc<dyn Callosum>,
        sink: Arc<dyn Fn(Event) + Send + Sync>,
        now: fn() -> Timestamp,
    ) -> Arc<Pairing> {
        Arc::new(Pairing {
            callosum,
            sink,
            now,
            watches: Mutex::new(HashMap::new()),
        })
    }

    /// How many watches run now.
    pub fn watching(&self) -> usize {
        self.watches.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Starts `command`. Reading the groups and recording a result on
    /// Devices belong to the live state, not to callosum, and are left to
    /// the caller.
    pub fn run(self: &Arc<Self>, command: Command) {
        match command {
            Command::Open { gen, group } => {
                let this = Arc::clone(self);
                tokio::spawn(async move {
                    let params = json!({ "class": CLASS, "expose_profile": group });
                    let reply = this
                        .callosum
                        .call("callosum.pairing_qr_open", params, OPEN_TIMEOUT)
                        .await;
                    let result = open_result(reply, (this.now)());
                    (this.sink)(Event::Opened { gen, result });
                });
            }
            Command::Confirm {
                window_id,
                words,
                timeout,
            } => {
                let this = Arc::clone(self);
                tokio::spawn(async move {
                    let params = json!({ "window_id": window_id, "words": words });
                    let reply = this
                        .callosum
                        .call("callosum.pairing_qr_confirm", params, timeout)
                        .await;
                    (this.sink)(Event::Confirmed {
                        window_id,
                        result: confirm_result(reply),
                    });
                });
            }
            Command::Watch { window_id } => self.watch(window_id),
            Command::Unwatch { window_id } => {
                let stop = self
                    .watches
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&window_id);
                if let Some(stop) = stop {
                    let _ = stop.send(());
                }
            }
            Command::ReadGroups | Command::Record { .. } => {}
        }
    }

    /// A watch that ended on its own no longer needs its stop switch.
    fn forget(&self, window_id: &str) {
        self.watches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(window_id);
    }

    fn watch(self: &Arc<Self>, window_id: String) {
        let (stop_tx, mut stop) = oneshot::channel();
        self.watches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(window_id.clone(), stop_tx);
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut stream = tokio::select! {
                opened = this.callosum.watch(window_id.clone()) => match opened {
                    Ok(stream) => stream,
                    Err(_) => {
                        this.forget(&window_id);
                        (this.sink)(Event::WatchEnded { window_id });
                        return;
                    }
                },
                _ = &mut stop => return,
            };
            loop {
                tokio::select! {
                    next = stream.next() => match next {
                        Some(value) => {
                            let event = match watch_event(&value) {
                                Ok(Some(event)) => event,
                                Ok(None) => continue,
                                // ck stops at a notice it can't read rather
                                // than guess what it meant.
                                Err(_) => {
                                    this.forget(&window_id);
                                    stream.close().await;
                                    (this.sink)(Event::WatchUnreadable { window_id });
                                    return;
                                }
                            };
                            let terminal = matches!(
                                event,
                                WatchEvent::Granted | WatchEvent::Declined | WatchEvent::Expired
                            );
                            (this.sink)(Event::Watch {
                                window_id: window_id.clone(),
                                event,
                            });
                            if terminal {
                                this.forget(&window_id);
                                stream.close().await;
                                return;
                            }
                        }
                        None => {
                            this.forget(&window_id);
                            stream.close().await;
                            (this.sink)(Event::WatchEnded { window_id });
                            return;
                        }
                    },
                    _ = &mut stop => {
                        stream.close().await;
                        return;
                    }
                }
            }
        });
    }
}

/// An error callosum put inside a reply: its code, message and the wait a
/// backoff names.
fn reply_refusal(reply: &Value) -> Option<Refusal> {
    let e = reply.get("error").filter(|e| !e.is_null())?;
    let text = |key: &str| e.get(key).and_then(Value::as_str).map(str::to_string);
    Some(Refusal {
        code: text("code")
            .or_else(|| e.as_str().map(str::to_string))
            .unwrap_or_else(|| "error".into()),
        message: text("message").unwrap_or_default(),
        retry_after_ms: e
            .get("detail")
            .and_then(|d| d.get("retry_after_ms"))
            .and_then(Value::as_u64),
    })
}

/// A `callosum.pairing_qr_open` reply as the exchange reads it.
pub fn open_result(
    reply: Result<Value, CallFailure>,
    received_at: Timestamp,
) -> Result<Opened, Refusal> {
    let reply = match reply {
        Ok(v) => v,
        Err(CallFailure::Refused(r)) => return Err(r),
        Err(CallFailure::Lost) => return Err(Refusal::code("callosum unreachable")),
    };
    if let Some(r) = reply_refusal(&reply) {
        return Err(r);
    }
    // Callosum sends the open's fields at the top level, unwrapped.
    let text = |key: &str| reply.get(key).and_then(Value::as_str).map(str::to_string);
    match (
        text("window_id"),
        text("qr_payload"),
        reply.get("expires_at_ms").and_then(Value::as_i64),
    ) {
        (Some(window_id), Some(qr_payload), Some(expires_at_ms)) => Ok(Opened {
            window_id,
            qr_payload,
            expires_at_ms,
            received_at,
        }),
        _ => Err(Refusal::code("unreadable reply")),
    }
}

/// A `callosum.pairing_qr_confirm` reply as the exchange reads it. A reply
/// that names no result leaves the outcome unknown.
pub fn confirm_result(reply: Result<Value, CallFailure>) -> Result<Verdict, ConfirmFailure> {
    let reply = match reply {
        Ok(v) => v,
        Err(CallFailure::Refused(r)) => return Err(ConfirmFailure::Refused(r)),
        Err(CallFailure::Lost) => return Err(ConfirmFailure::Lost),
    };
    if let Some(r) = reply_refusal(&reply) {
        return Err(ConfirmFailure::Refused(r));
    }
    match reply.get("result").and_then(Value::as_str) {
        Some("granted") => Ok(Verdict::Granted),
        Some("mismatch") => Ok(Verdict::Mismatch),
        _ => Err(ConfirmFailure::Lost),
    }
}

/// A watch notice ck can't read: a known state missing a field it requires,
/// or carrying one in the wrong form, or a notice that isn't an object with
/// a string `state`. The text says which, for diagnosis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unreadable(pub String);

/// One `callosum.pairing_qr_watch` notice: a JSON object whose `state` is
/// `open`, `foreign_attempt {source_addr}`, `claimed {device_name,
/// source_addr, fingerprint}`, `granted`, `declined` or `expired`. Notices
/// arrive unwrapped, exactly as callosum builds them. A state ck doesn't know
/// is skipped (`Ok(None)`); a known state with a missing or invalid field is
/// unreadable, and ck fills in nothing for it.
pub fn watch_event(value: &Value) -> Result<Option<WatchEvent>, Unreadable> {
    let bad = |why: &str| Unreadable(why.to_string());
    let Some(state) = value.get("state").and_then(Value::as_str) else {
        return Err(bad("no string state"));
    };
    // Callosum sends an address without a port, so anything else is
    // unreadable rather than shown in part.
    let addr = || -> Result<String, Unreadable> {
        let text = value
            .get("source_addr")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("no source_addr"))?;
        text.parse::<std::net::IpAddr>()
            .map_err(|_| bad("source_addr is not an IP address"))?;
        Ok(text.to_string())
    };
    Ok(Some(match state {
        "open" => WatchEvent::Open,
        "foreign_attempt" => WatchEvent::ForeignAttempt(addr()?),
        "claimed" => {
            let device_name = match value.get("device_name") {
                Some(Value::String(name)) => Some(name.clone()),
                // Callosum's registry has no name for the phone.
                Some(Value::Null) => None,
                _ => return Err(bad("device_name is neither a string nor null")),
            };
            let fingerprint = value
                .get("fingerprint")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("no fingerprint"))?;
            let hex = fingerprint.len() == 32
                && fingerprint
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if !hex {
                return Err(bad("fingerprint is not 32 lowercase hex characters"));
            }
            WatchEvent::Claimed(Claim {
                device_name,
                source_addr: addr()?,
                fingerprint: fingerprint.to_string(),
            })
        }
        "granted" => WatchEvent::Granted,
        "declined" => WatchEvent::Declined,
        "expired" => WatchEvent::Expired,
        _ => return Ok(None),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    /// A callosum whose watches stay open, never sending an event, and
    /// whose calls answer at once.
    struct Quiet {
        calls: Mutex<Vec<&'static str>>,
        closed: Arc<Mutex<Vec<String>>>,
    }

    struct Silent {
        window_id: String,
        closed: Arc<Mutex<Vec<String>>>,
    }

    impl WatchStream for Silent {
        fn next(&mut self) -> BoxFuture<'_, Option<Value>> {
            Box::pin(std::future::pending())
        }
        fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
            self.closed.lock().unwrap().push(self.window_id.clone());
            Box::pin(async {})
        }
    }

    impl Callosum for Quiet {
        fn call(
            &self,
            method: &'static str,
            _params: Value,
            _timeout: Duration,
        ) -> BoxFuture<'static, Result<Value, CallFailure>> {
            self.calls.lock().unwrap().push(method);
            let reply = match method {
                "callosum.pairing_qr_confirm" => json!({ "result": "granted" }),
                _ => json!({ "devices": [], "self": {} }),
            };
            Box::pin(async move { Ok(reply) })
        }

        fn watch(
            &self,
            window_id: String,
        ) -> BoxFuture<'static, Result<Box<dyn WatchStream>, CallFailure>> {
            let closed = Arc::clone(&self.closed);
            Box::pin(
                async move { Ok(Box::new(Silent { window_id, closed }) as Box<dyn WatchStream>) },
            )
        }
    }

    fn rig() -> (Arc<Pairing>, Arc<Quiet>, mpsc::UnboundedReceiver<Event>) {
        let quiet = Arc::new(Quiet {
            calls: Mutex::default(),
            closed: Arc::default(),
        });
        let (tx, rx) = mpsc::unbounded_channel();
        let pairing = Pairing::new(
            Arc::clone(&quiet) as Arc<dyn Callosum>,
            Arc::new(move |e| {
                let _ = tx.send(e);
            }),
            || Timestamp::from_unix_millis(0),
        );
        (pairing, quiet, rx)
    }

    #[tokio::test(start_paused = true)]
    async fn a_confirm_completes_while_two_watches_are_held() {
        let (pairing, quiet, mut rx) = rig();
        for id in ["w-a", "w-b"] {
            pairing.run(Command::Watch {
                window_id: id.into(),
            });
        }
        tokio::task::yield_now().await;
        assert_eq!(pairing.watching(), 2);
        pairing.run(Command::Confirm {
            window_id: "w-c".into(),
            words: ["a", "b", "c", "d"].map(String::from),
            timeout: Duration::from_secs(300),
        });
        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("the confirm answers within the normal call timeout");
        assert_eq!(
            event,
            Some(Event::Confirmed {
                window_id: "w-c".into(),
                result: Ok(Verdict::Granted),
            })
        );
        let reply = quiet
            .call("callosum.devices_read", json!({}), OPEN_TIMEOUT)
            .await;
        assert!(
            reply.is_ok(),
            "devices_read answers while both watches stay held"
        );
        assert_eq!(pairing.watching(), 2, "both watches are still held");
    }

    #[tokio::test(start_paused = true)]
    async fn unwatch_closes_the_watch_route() {
        let (pairing, quiet, _rx) = rig();
        pairing.run(Command::Watch {
            window_id: "w-a".into(),
        });
        tokio::task::yield_now().await;
        pairing.run(Command::Unwatch {
            window_id: "w-a".into(),
        });
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(pairing.watching(), 0);
        assert_eq!(*quiet.closed.lock().unwrap(), ["w-a"]);
    }

    #[test]
    fn a_confirm_reply_without_a_result_is_an_unknown_outcome() {
        assert_eq!(
            confirm_result(Ok(json!({"result": "granted"}))),
            Ok(Verdict::Granted)
        );
        assert_eq!(
            confirm_result(Ok(json!({"result": "mismatch"}))),
            Ok(Verdict::Mismatch)
        );
        assert_eq!(confirm_result(Ok(json!({}))), Err(ConfirmFailure::Lost));
    }

    #[test]
    fn an_open_refusal_keeps_its_message() {
        let refused = open_result(
            Ok(
                json!({"error": {"code": "fed_bad_request", "message": "unknown expose profile \"ops\""}}),
            ),
            Timestamp::from_unix_millis(0),
        );
        assert_eq!(
            refused,
            Err(Refusal {
                code: "fed_bad_request".into(),
                message: "unknown expose profile \"ops\"".into(),
                retry_after_ms: None,
            })
        );
    }
}
