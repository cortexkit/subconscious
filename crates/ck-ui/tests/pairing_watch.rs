//! `callosum.pairing_qr_watch` notices as callosum builds them. The notices
//! live as files under `tests/pairing-qr-watch/`, sorted by what ck must do
//! with them: `valid/` ones are read, `skipped/` ones are ignored, and
//! `malformed/` ones stop the overlay. Swapping in another set of vectors
//! means replacing those files.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ck_ui::app::{App, Overlay, Request, Tab};
use ck_ui::live::pairing::{watch_event, CallFailure, Callosum, Pairing, WatchStream};
use ck_ui::model::Snapshot;
use ck_ui::pairing::flow::{Command, Event, WatchEvent};
use ck_ui::pairing::Claim;
use ck_ui::Theme;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use futures_util::future::BoxFuture;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use serde_json::{json, Value};

const VECTORS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/pairing-qr-watch");
const FINGERPRINT: &str = "474456dd9c0e21b8a1f03c5e7d92b6f4";

fn vector(name: &str) -> Value {
    let path = format!("{VECTORS}/{name}");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn read(name: &str) -> Option<WatchEvent> {
    watch_event(&vector(name)).unwrap_or_else(|e| panic!("{name} is unreadable: {e:?}"))
}

fn files(folder: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(format!("{VECTORS}/{folder}"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".json"))
        .map(|n| format!("{folder}/{n}"))
        .collect();
    names.sort();
    names
}

// ---- one test per state ----------------------------------------------------------

#[test]
fn open_is_read() {
    assert_eq!(read("valid/open.json"), Some(WatchEvent::Open));
}

#[test]
fn foreign_attempt_is_read_with_its_address() {
    assert_eq!(
        read("valid/foreign_attempt.json"),
        Some(WatchEvent::ForeignAttempt("192.168.1.77".into()))
    );
}

#[test]
fn claimed_is_read_with_its_name_address_and_fingerprint() {
    assert_eq!(
        read("valid/claimed.json"),
        Some(WatchEvent::Claimed(Claim {
            device_name: Some("iPhone".into()),
            source_addr: "192.168.1.41".into(),
            fingerprint: FINGERPRINT.into(),
        }))
    );
    assert_eq!(
        read("valid/claimed-ipv6.json"),
        Some(WatchEvent::Claimed(Claim {
            device_name: Some("iPhone".into()),
            source_addr: "2001:db8::41".into(),
            fingerprint: FINGERPRINT.into(),
        }))
    );
}

#[test]
fn claimed_with_a_null_name_has_no_name() {
    assert_eq!(
        read("valid/claimed-null-name.json"),
        Some(WatchEvent::Claimed(Claim {
            device_name: None,
            source_addr: "192.168.1.41".into(),
            fingerprint: FINGERPRINT.into(),
        }))
    );
}

#[test]
fn granted_is_read() {
    assert_eq!(read("valid/granted.json"), Some(WatchEvent::Granted));
}

#[test]
fn declined_is_read() {
    assert_eq!(read("valid/declined.json"), Some(WatchEvent::Declined));
}

#[test]
fn expired_is_read() {
    assert_eq!(read("valid/expired.json"), Some(WatchEvent::Expired));
}

#[test]
fn an_unknown_state_is_skipped() {
    assert_eq!(read("skipped/unknown-state.json"), None);
}

// ---- malformed notices ---------------------------------------------------------------

fn unreadable(name: &str) -> bool {
    watch_event(&vector(name)).is_err()
}

#[test]
fn claimed_without_a_fingerprint_is_unreadable() {
    assert!(unreadable("malformed/claimed-without-fingerprint.json"));
}

#[test]
fn a_31_character_fingerprint_is_unreadable() {
    assert!(unreadable("malformed/claimed-fingerprint-31-chars.json"));
}

#[test]
fn an_uppercase_fingerprint_is_unreadable() {
    assert!(unreadable("malformed/claimed-fingerprint-uppercase.json"));
}

#[test]
fn a_source_addr_with_a_port_is_unreadable() {
    assert!(unreadable("malformed/claimed-source-addr-with-port.json"));
    assert!(unreadable(
        "malformed/foreign-attempt-source-addr-with-port.json"
    ));
}

#[test]
fn the_old_event_keyed_form_is_unreadable() {
    assert!(unreadable("malformed/event-keyed.json"));
}

#[test]
fn every_vector_file_is_read_as_its_folder_says() {
    let (valid, skipped, malformed) = (files("valid"), files("skipped"), files("malformed"));
    assert!(!valid.is_empty() && !skipped.is_empty() && !malformed.is_empty());
    for name in valid {
        assert!(
            matches!(watch_event(&vector(&name)), Ok(Some(_))),
            "{name} wasn't read"
        );
    }
    for name in skipped {
        assert_eq!(watch_event(&vector(&name)), Ok(None), "{name}");
    }
    for name in malformed {
        assert!(unreadable(&name), "{name} was read");
    }
}

// ---- through the live executor to the overlay -------------------------------------------

/// A callosum whose open answers with window `w-1` and whose watch sends
/// the given notices, then waits.
struct Scripted {
    notices: Mutex<Vec<Value>>,
    closed: Arc<Mutex<u32>>,
    timeouts: Mutex<Vec<(&'static str, Duration)>>,
}

struct Notices {
    left: Vec<Value>,
    closed: Arc<Mutex<u32>>,
}

impl WatchStream for Notices {
    fn next(&mut self) -> BoxFuture<'_, Option<Value>> {
        Box::pin(async move {
            if self.left.is_empty() {
                std::future::pending::<()>().await;
            }
            Some(self.left.remove(0))
        })
    }
    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        *self.closed.lock().unwrap() += 1;
        Box::pin(async {})
    }
}

impl Callosum for Scripted {
    fn call(
        &self,
        method: &'static str,
        _params: Value,
        timeout: Duration,
    ) -> BoxFuture<'static, Result<Value, CallFailure>> {
        self.timeouts.lock().unwrap().push((method, timeout));
        let reply = match method {
            "callosum.pairing_qr_open" => json!({
                "window_id": "w-1",
                "qr_payload": format!("ckpair:1/{}", "b".repeat(123)),
                "expires_at_ms": Snapshot::fixture_now().unix_millis() + 600_000,
            }),
            _ => json!({ "result": "granted" }),
        };
        Box::pin(async move { Ok(reply) })
    }

    fn watch(
        &self,
        _window_id: String,
    ) -> BoxFuture<'static, Result<Box<dyn WatchStream>, CallFailure>> {
        let left = std::mem::take(&mut *self.notices.lock().unwrap());
        let closed = Arc::clone(&self.closed);
        Box::pin(async move { Ok(Box::new(Notices { left, closed }) as Box<dyn WatchStream>) })
    }
}

/// Opens the overlay on an app whose calls go to a scripted callosum
/// sending `notices` on the window's watch, and runs until the exchange
/// settles. Returns the app, the callosum and the screen at 120×36.
async fn through_the_executor(notices: Vec<Value>) -> (App, Arc<Scripted>, String) {
    let callosum = Arc::new(Scripted {
        notices: Mutex::new(notices),
        closed: Arc::default(),
        timeouts: Mutex::default(),
    });
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let pairing = Pairing::new(
        Arc::clone(&callosum) as Arc<dyn Callosum>,
        Arc::new(move |e| {
            let _ = tx.send(e);
        }),
        Snapshot::fixture_now,
    );
    let mut app = App::new(Theme::test());
    app.tab = Tab::Devices;
    let mut snap = Snapshot::fixture();
    let now = Snapshot::fixture_now();
    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &snap);
    assert_eq!(app.overlay, Some(Overlay::Pairing));
    let mut seq = 0;
    loop {
        for r in app.requests.drain(..) {
            if let Request::Pairing(c) = r {
                pairing.run(c);
            }
        }
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(event)) => {
                seq += 1;
                snap.pairing.push((seq, event));
                app.on_snapshot(&snap, now);
            }
            _ => break,
        }
    }
    let mut term = Terminal::new(TestBackend::new(120, 36)).unwrap();
    term.draw(|f| app.render(f.buffer_mut(), &snap, now))
        .unwrap();
    let buf = term.backend().buffer().clone();
    let screen = (0..36)
        .map(|y| (0..120).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    (app, callosum, screen)
}

#[tokio::test(start_paused = true)]
async fn a_malformed_notice_stops_the_overlay_with_a_plain_error() {
    let (app, callosum, screen) = through_the_executor(vec![
        vector("valid/open.json"),
        vector("malformed/claimed-without-fingerprint.json"),
    ])
    .await;
    assert_eq!(
        app.pairing.status().as_deref(),
        Some("Callosum sent a pairing update ck can't read")
    );
    assert!(screen.contains("Callosum sent a pairing update ck can't read"));
    for guess in ["A phone", "an unknown address", "scanned the code"] {
        assert!(!screen.contains(guess), "{guess} was filled in:\n{screen}");
    }
    assert_eq!(*callosum.closed.lock().unwrap(), 1, "the watch stays open");
    assert_eq!(app.pairing.watched(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_claim_without_a_name_reads_no_name() {
    let (app, _callosum, screen) = through_the_executor(vec![
        vector("valid/open.json"),
        vector("valid/claimed-null-name.json"),
    ])
    .await;
    let claim = app.pairing.window.as_ref().unwrap().claim.clone().unwrap();
    assert_eq!(claim.device_name, None);
    assert!(screen.contains("(no name) scanned the code."), "{screen}");
    assert!(!screen.contains("A phone"));
    assert!(screen.contains(&format!("key {}", &FINGERPRINT[..16])));
}

#[tokio::test(start_paused = true)]
async fn the_confirm_is_sent_with_a_300_s_timeout() {
    let callosum = Arc::new(Scripted {
        notices: Mutex::default(),
        closed: Arc::default(),
        timeouts: Mutex::default(),
    });
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let pairing = Pairing::new(
        Arc::clone(&callosum) as Arc<dyn Callosum>,
        Arc::new(move |e: Event| {
            let _ = tx.send(e);
        }),
        Snapshot::fixture_now,
    );
    pairing.run(Command::Confirm {
        window_id: "w-1".into(),
        words: ["a", "b", "c", "d"].map(String::from),
        timeout: Duration::from_secs(300),
    });
    let _ = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
    assert_eq!(
        *callosum.timeouts.lock().unwrap(),
        [("callosum.pairing_qr_confirm", Duration::from_secs(300))]
    );
}
