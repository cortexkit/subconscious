use ck_ui::computer::{self, Command, Event, Flow};
use ck_ui::live::computer::Computer;
use ck_ui::live::pairing::{CallFailure, Callosum, WatchStream};
use ck_ui::model::Devices;
use ck_ui::pairing::flow::Refusal;
use crossterm::event::KeyCode;
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SAMPLES: &str = include_str!("fixtures/computer-pairing-ops-v1/cases.json");
fn sample(id: &str) -> Value {
    serde_json::from_str::<Value>(SAMPLES).unwrap()["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap()
        .clone()
}

#[test]
fn published_samples_are_byte_exact() {
    assert_eq!(
        format!("{:x}", Sha256::digest(SAMPLES.as_bytes())),
        "7361385473af89afedbd80df57a3f8a8ee31371243b7ff2c79f16a0a30f56240"
    );
}

#[test]
fn picker_skips_unavailable_and_uses_fingerprint_after_refresh() {
    let list = computer::candidates(&sample("candidates_synced")["reply"]).unwrap();
    let mut flow = Flow::default();
    flow.open();
    let Command::Candidates { ticket } = flow.outbox.remove(0) else {
        panic!()
    };
    flow.absorb(
        Event::Candidates {
            ticket,
            result: Ok(list.clone()),
        },
        &Devices::default(),
    );
    assert_eq!(flow.picked().unwrap().name, "Build box");
    flow.key(KeyCode::Down, &Devices::default());
    assert_eq!(flow.picked().unwrap().name, "Studio PC");
    flow.tick(10_000);
    let Command::Candidates { ticket } = flow.outbox.remove(0) else {
        panic!()
    };
    let mut reversed = list.1;
    reversed.reverse();
    flow.absorb(
        Event::Candidates {
            ticket,
            result: Ok((false, reversed)),
        },
        &Devices::default(),
    );
    assert_eq!(flow.picked().unwrap().name, "Studio PC");
    flow.key(KeyCode::Esc, &Devices::default());
    flow.absorb(
        Event::Candidates {
            ticket,
            result: Ok((false, vec![])),
        },
        &Devices::default(),
    );
    assert!(!flow.picker);
}

#[test]
fn partial_forget_requires_retry_before_not_paired_is_success() {
    let mut flow = Flow::default();
    flow.detail = Some("9f72ea0cf49536e3c66c787f705186df".into());
    flow.forget = true;
    flow.key(KeyCode::Char('x'), &Devices::default());
    let Command::Forget { ticket, .. } = flow.outbox.remove(0) else {
        panic!()
    };
    flow.absorb(
        Event::Forgotten {
            ticket,
            result: Err(Refusal::code("fed_not_paired")),
        },
        &Devices::default(),
    );
    assert!(flow.forget && flow.notice.is_none());
    flow.key(KeyCode::Char('x'), &Devices::default());
    let Command::Forget { ticket, .. } = flow.outbox.remove(0) else {
        panic!()
    };
    flow.absorb(
        Event::Forgotten {
            ticket,
            result: Err(Refusal::code("fed_target_unavailable")),
        },
        &Devices::default(),
    );
    assert!(flow.partial);
    flow.key(KeyCode::Char('x'), &Devices::default());
    let Command::Forget { ticket, .. } = flow.outbox.remove(0) else {
        panic!()
    };
    flow.absorb(
        Event::Forgotten {
            ticket,
            result: Err(Refusal::code("fed_not_paired")),
        },
        &Devices::default(),
    );
    assert!(!flow.forget && flow.detail.is_none());
    assert!(flow.notice.unwrap().contains("Already forgotten"));
    assert_eq!(flow.outbox, vec![Command::ReadDevices]);
}

#[test]
fn wrong_reply_shapes_are_not_successes() {
    for bad in [
        json!({"result": {"registry":"synced", "devices":[]}}),
        json!({"devices":[]}),
        json!({"registry":"unknown", "devices":[]}),
        json!({"registry":"waiting", "devices":[{}]}),
    ] {
        assert!(computer::candidates(&bad).is_err());
    }
    for bad in [
        Value::Null,
        json!([]),
        json!({"result":{}}),
        json!({"ok":true}),
    ] {
        assert!(computer::empty_reply(&bad).is_err());
    }
}

struct Script(Mutex<VecDeque<Value>>);
impl Callosum for Script {
    fn watch_computer(
        &self,
        window_id: u64,
    ) -> BoxFuture<'static, Result<Box<dyn WatchStream>, CallFailure>> {
        let case = self.0.lock().unwrap().pop_front().unwrap();
        assert_eq!(
            json!({"method":"callosum.pairing_computer_watch","params":{"window_id":window_id}}),
            case["request"]
        );
        let events = case["events"].as_array().unwrap().clone();
        Box::pin(async move { Ok(Box::new(Events(events.into())) as Box<dyn WatchStream>) })
    }
    fn call(
        &self,
        method: &'static str,
        params: Value,
        _: Duration,
    ) -> BoxFuture<'static, Result<Value, CallFailure>> {
        let case = self.0.lock().unwrap().pop_front().unwrap();
        assert_eq!(json!({"method":method,"params":params}), case["request"]);
        Box::pin(async move {
            if let Some(error) = case.get("error") {
                Err(CallFailure::Refused(Refusal {
                    code: error["code"].as_str().unwrap().into(),
                    message: error["message"].as_str().unwrap().into(),
                    retry_after_ms: None,
                }))
            } else {
                Ok(case["reply"].clone())
            }
        })
    }
    fn watch(&self, _: String) -> BoxFuture<'static, Result<Box<dyn WatchStream>, CallFailure>> {
        Box::pin(async { panic!("unexpected phone watch") })
    }
}

#[tokio::test]
async fn live_picker_and_forget_drive_published_bodies() {
    let script = Arc::new(Script(Mutex::new(VecDeque::from([
        sample("candidates_synced"),
        sample("unpair_success"),
    ]))));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let executor = Computer::new(
        script.clone(),
        Arc::new(move |e| {
            tx.send(e).unwrap();
        }),
    );
    executor.run(Command::Candidates { ticket: 1 });
    let Event::Candidates { result, .. } = rx.recv().await.unwrap() else {
        panic!()
    };
    assert_eq!(result.unwrap().1.len(), 5);
    executor.run(Command::Forget {
        ticket: 2,
        fingerprint: "9f72ea0cf49536e3c66c787f705186df".into(),
    });
    assert!(matches!(
        rx.recv().await.unwrap(),
        Event::Forgotten {
            ticket: 2,
            result: Ok(())
        }
    ));
    assert!(script.0.lock().unwrap().is_empty());
}

fn opened_flow() -> Flow {
    let mut flow = Flow::default();
    flow.open();
    let Command::Candidates { ticket } = flow.outbox.remove(0) else {
        panic!()
    };
    flow.absorb(
        Event::Candidates {
            ticket,
            result: computer::candidates(&sample("candidates_synced")["reply"]),
        },
        &Devices::default(),
    );
    flow.key(KeyCode::Enter, &Devices::default());
    let Command::Open { ticket, .. } = flow.outbox.remove(0) else {
        panic!()
    };
    flow.absorb(
        Event::Opened {
            ticket,
            result: computer::opened(&sample("computer_open_success")["reply"]),
        },
        &Devices::default(),
    );
    flow.outbox.clear();
    flow
}

#[test]
fn six_digit_ceremony_hides_peer_answer_and_enter_never_confirms() {
    let mut flow = opened_flow();
    let events = sample("watch_live_to_paired")["events"]
        .as_array()
        .unwrap()
        .clone();
    for event in events.iter().take(4) {
        flow.absorb(
            Event::Watch {
                window_id: 4294967297,
                state: computer::watch(event).unwrap(),
            },
            &Devices::default(),
        );
    }
    assert!(matches!(
        flow.session.as_ref().unwrap().state,
        computer::Watch::Code { .. }
    ));
    assert!(!flow.session.as_ref().unwrap().answered);
    flow.key(KeyCode::Enter, &Devices::default());
    assert!(flow.outbox.is_empty());
    flow.key(KeyCode::Char('y'), &Devices::default());
    assert_eq!(
        flow.outbox,
        vec![Command::Confirm {
            window_id: 4294967297,
            matches: true
        }]
    );
    flow.key(KeyCode::Char('y'), &Devices::default());
    assert_eq!(flow.outbox.len(), 1);
    flow.absorb(
        Event::Confirmed {
            window_id: 4294967297,
            matches: true,
            result: Ok(()),
        },
        &Devices::default(),
    );
    assert!(flow.session.as_ref().unwrap().answered);
    flow.absorb(
        Event::Watch {
            window_id: 4294967297,
            state: computer::watch(&events[4]).unwrap(),
        },
        &Devices::default(),
    );
    assert!(flow.session.as_ref().unwrap().state.terminal());
}

#[test]
fn cancellation_and_watch_retry_keep_window_identity() {
    let mut flow = opened_flow();
    flow.tick(100);
    flow.absorb(
        Event::WatchLost {
            window_id: 4294967297,
        },
        &Devices::default(),
    );
    flow.tick(3_099);
    assert!(flow.outbox.is_empty());
    flow.tick(3_100);
    assert_eq!(
        flow.outbox,
        vec![Command::Watch {
            window_id: 4294967297
        }]
    );
    flow.outbox.clear();
    flow.key(KeyCode::Esc, &Devices::default());
    assert_eq!(
        flow.outbox,
        vec![
            Command::Cancel {
                window_id: 4294967297
            },
            Command::Unwatch {
                window_id: 4294967297
            }
        ]
    );
    flow.absorb(
        Event::Watch {
            window_id: 4294967297,
            state: computer::Watch::Expired { peer: true },
        },
        &Devices::default(),
    );
    assert!(flow.session.is_none());
}

#[test]
fn every_published_ceremony_state_and_wrong_shapes_are_checked() {
    let samples: Value = serde_json::from_str(SAMPLES).unwrap();
    for case in samples["cases"].as_array().unwrap() {
        if let Some(events) = case.get("events").and_then(Value::as_array) {
            for event in events {
                assert!(computer::watch(event).is_ok(), "{}: {event}", case["id"]);
            }
        }
    }
    for bad in [
        json!({"state":"code","code":"082917","confirm_by_ms":4000000120000i64}),
        json!({"state":"code","code":"082 917"}),
        json!({"state":"paired","name":"Build box"}),
        json!({"state":"declined"}),
        json!({"state":"expired","by":"unknown"}),
        json!({"state":"failed"}),
        json!({"result":{"state":"connecting"}}),
    ] {
        assert!(computer::watch(&bad).is_err());
    }
    assert!(
        computer::opened(&json!({"window_id":"4294967297","expires_at_ms":4000000300000i64}))
            .is_err()
    );
}

#[test]
fn failed_open_refreshes_list_and_late_open_is_cancelled() {
    let mut flow = Flow::default();
    flow.open();
    let Command::Candidates { ticket } = flow.outbox.remove(0) else {
        panic!()
    };
    flow.absorb(
        Event::Candidates {
            ticket,
            result: computer::candidates(&sample("candidates_synced")["reply"]),
        },
        &Devices::default(),
    );
    flow.key(KeyCode::Enter, &Devices::default());
    let Command::Open { ticket, .. } = flow.outbox.remove(0) else {
        panic!()
    };
    flow.absorb(
        Event::Opened {
            ticket,
            result: Err(Refusal::code("fed_bad_request")),
        },
        &Devices::default(),
    );
    assert!(
        flow.picker
            && flow
                .refreshed
                .as_ref()
                .unwrap()
                .contains("Build box on Linux can't be paired")
    );
    assert!(matches!(flow.outbox[0], Command::Candidates { .. }));
    flow.outbox.clear();
    flow.key(KeyCode::Esc, &Devices::default());
    flow.absorb(
        Event::Opened {
            ticket,
            result: computer::opened(&sample("computer_open_success")["reply"]),
        },
        &Devices::default(),
    );
    assert_eq!(
        flow.outbox,
        vec![Command::Cancel {
            window_id: 4294967297
        }]
    );
}

struct Events(VecDeque<Value>);
impl WatchStream for Events {
    fn next(&mut self) -> BoxFuture<'_, Option<Value>> {
        Box::pin(async { self.0.pop_front() })
    }
    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

#[tokio::test]
async fn live_ceremony_drives_numeric_ids_and_published_event_bodies() {
    let script = Arc::new(Script(Mutex::new(VecDeque::from([
        sample("computer_open_success"),
        sample("computer_confirm_success"),
        sample("watch_live_to_paired"),
        sample("cancel_computer_window"),
    ]))));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let executor = Computer::new(
        script.clone(),
        Arc::new(move |e| {
            tx.send(e).unwrap();
        }),
    );
    executor.run(Command::Open {
        ticket: 1,
        fingerprint: "02d449a31fbb267c8f352e9968a79e3e".into(),
    });
    let Event::Opened { result, .. } = rx.recv().await.unwrap() else {
        panic!()
    };
    assert_eq!(result.unwrap().window_id, 4294967297);
    executor.run(Command::Confirm {
        window_id: 4294967297,
        matches: true,
    });
    assert!(matches!(
        rx.recv().await.unwrap(),
        Event::Confirmed { result: Ok(()), .. }
    ));
    executor.run(Command::Watch {
        window_id: 4294967297,
    });
    for _ in 0..5 {
        assert!(matches!(
            rx.recv().await.unwrap(),
            Event::Watch {
                window_id: 4294967297,
                ..
            }
        ));
    }
    executor.run(Command::Cancel {
        window_id: 4294967298,
    });
    tokio::task::yield_now().await;
    assert!(script.0.lock().unwrap().is_empty());
}

fn shared_devices() -> Devices {
    ck_ui::live::map::devices(
        &sample("devices_shares_on")["notices"][0],
        ck_ui::Snapshot::fixture_now(),
    )
    .unwrap()
}

#[test]
fn inventory_notices_decode_named_switches_and_optional_pairing_facts() {
    let mut notice = sample("devices_shares_on")["notices"][0].clone();
    let devices = shared_devices();
    let details = devices.inventory.computers.values().next().unwrap();
    assert_eq!(details.shares.as_ref().unwrap()[0].module, "claustrum");
    assert!(details.platform.is_none() && details.pairing.is_none());
    notice["devices"][0]["platform"] = json!("windows");
    notice["devices"][0]["pairing"] = json!("granted_here");
    let mapped = ck_ui::live::map::devices(&notice, ck_ui::Snapshot::fixture_now()).unwrap();
    let details = mapped.inventory.computers.values().next().unwrap();
    assert_eq!(details.platform.as_deref(), Some("windows"));
    assert_eq!(details.pairing.as_deref(), Some("granted_here"));
    notice["devices"][0]["shares"][0]["enabled"] = json!("true");
    assert!(ck_ui::live::map::devices(&notice, ck_ui::Snapshot::fixture_now()).is_err());
    notice["devices"][0]["shares"] = json!({"vault":true});
    assert!(ck_ui::live::map::devices(&notice, ck_ui::Snapshot::fixture_now()).is_err());
}

/// The published samples carry the computer's system and its pairing state
/// as Callosum serves them. A computer paired the older way carries neither,
/// so its details must not claim either fact.
#[test]
fn published_pairing_facts_decode_and_absent_means_unknown() {
    let facts = |id: &str| {
        let devices =
            ck_ui::live::map::devices(&sample(id)["notices"][0], ck_ui::Snapshot::fixture_now())
                .unwrap();
        let mut facts: Vec<_> = devices
            .inventory
            .computers
            .values()
            .map(|d| (d.platform.clone(), d.pairing.clone()))
            .collect();
        facts.sort();
        facts
    };
    let linux = || Some("linux".to_string());
    assert_eq!(
        facts("devices_pairing_granted_here"),
        vec![(linux(), Some("granted_here".to_string()))]
    );
    assert_eq!(
        facts("devices_pairing_confirmed_both"),
        vec![(linux(), Some("confirmed_both".to_string()))]
    );
    assert_eq!(
        facts("devices_pairing_legacy_unknown"),
        vec![(None, None), (linux(), Some("confirmed_both".to_string()))]
    );
}

#[test]
fn switches_follow_names_across_refresh_and_success_waits_for_inventory() {
    let mut devices = shared_devices();
    let id = devices.inventory.computers.keys().next().unwrap().clone();
    devices
        .inventory
        .computers
        .get_mut(&id)
        .unwrap()
        .shares
        .as_mut()
        .unwrap()
        .push(computer::Share {
            module: "motor".into(),
            share: "builds".into(),
            label: "Run builds on this computer".into(),
            enabled: false,
            needs_review: false,
        });
    let mut flow = Flow::default();
    flow.detail = Some(id.clone());
    flow.share_selected = Some(("motor".into(), "builds".into()));
    devices
        .inventory
        .computers
        .get_mut(&id)
        .unwrap()
        .shares
        .as_mut()
        .unwrap()
        .reverse();
    flow.key(KeyCode::Enter, &devices);
    let Command::Sharing { ticket, call } = flow.outbox.remove(0) else {
        panic!()
    };
    assert_eq!(call.module, "motor");
    assert_eq!(call.share, "builds");
    assert!(call.enabled);
    flow.key(KeyCode::Enter, &devices);
    assert!(flow.outbox.is_empty());
    flow.absorb(
        Event::Shared {
            ticket,
            result: Ok(()),
        },
        &devices,
    );
    assert!(!flow.shares(&devices)[flow.share_index(&devices)].enabled);
    assert_eq!(flow.outbox, vec![Command::ReadDevices]);
    flow.outbox.clear();
    let share = devices
        .inventory
        .computers
        .get_mut(&id)
        .unwrap()
        .shares
        .as_mut()
        .unwrap()
        .iter_mut()
        .find(|s| s.module == "motor")
        .unwrap();
    share.enabled = true;
    flow.key(KeyCode::Enter, &devices);
    assert!(matches!(&flow.outbox[0], Command::Sharing { call, .. } if !call.enabled));
}

#[test]
fn switch_refusals_backoff_and_review_do_not_guess_confirmation_method() {
    let mut devices = shared_devices();
    let id = devices.inventory.computers.keys().next().unwrap().clone();
    let mut flow = Flow::default();
    flow.detail = Some(id.clone());
    let share = &mut devices
        .inventory
        .computers
        .get_mut(&id)
        .unwrap()
        .shares
        .as_mut()
        .unwrap()[0];
    share.enabled = true;
    share.needs_review = true;
    flow.key(KeyCode::Enter, &devices);
    let Command::Sharing { ticket, call } = flow.outbox.remove(0) else {
        panic!()
    };
    assert!(
        call.enabled,
        "review asks to turn on new operations, not turn off the old grant"
    );
    flow.absorb(
        Event::Shared {
            ticket,
            result: Err(Refusal {
                code: "fed_operator_confirmation_backoff".into(),
                message: "cooling down".into(),
                retry_after_ms: Some(25_000),
            }),
        },
        &devices,
    );
    flow.tick(24_999);
    flow.key(KeyCode::Enter, &devices);
    assert!(flow.outbox.is_empty());
    flow.tick(25_000);
    flow.key(KeyCode::Enter, &devices);
    let Command::Sharing { ticket, .. } = flow.outbox.remove(0) else {
        panic!()
    };
    flow.absorb(
        Event::Shared {
            ticket,
            result: Err(Refusal::code("fed_operator_confirmation_refused")),
        },
        &devices,
    );
    assert_eq!(
        flow.share_problem(&id, &flow.shares(&devices)[0])
            .as_deref(),
        Some("Confirmation declined · ⏎ try again")
    );
}

#[tokio::test]
async fn live_sharing_drives_published_on_off_and_refusal_bodies() {
    let ids = [
        "sharing_on",
        "sharing_off",
        "sharing_refused",
        "sharing_unknown_share",
        "sharing_not_paired",
    ];
    let script = Arc::new(Script(Mutex::new(
        ids.iter().map(|id| sample(id)).collect(),
    )));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let executor = Computer::new(
        script.clone(),
        Arc::new(move |e| {
            tx.send(e).unwrap();
        }),
    );
    for (i, id) in ids.iter().enumerate() {
        let params = &sample(id)["request"]["params"];
        executor.run(Command::Sharing {
            ticket: i as u64,
            call: computer::ShareCall {
                fingerprint: params["fingerprint"].as_str().unwrap().into(),
                module: params["module"].as_str().unwrap().into(),
                share: params["share"].as_str().unwrap().into(),
                label: "Use this computer's vault".into(),
                enabled: params["enabled"].as_bool().unwrap(),
            },
        });
        let Event::Shared { ticket, result } = rx.recv().await.unwrap() else {
            panic!()
        };
        assert_eq!(ticket, i as u64);
        assert_eq!(result.is_ok(), i < 2);
    }
    assert!(script.0.lock().unwrap().is_empty());
}

#[test]
fn picker_read_failure_hides_stale_choices_until_a_fresh_reply() {
    let mut flow = Flow::default();
    flow.open();
    let Command::Candidates { ticket } = flow.outbox.remove(0) else {
        panic!()
    };
    flow.absorb(
        Event::Candidates {
            ticket,
            result: computer::candidates(&sample("candidates_synced")["reply"]),
        },
        &Devices::default(),
    );
    flow.tick(10_000);
    let Command::Candidates { ticket } = flow.outbox.remove(0) else {
        panic!()
    };
    flow.absorb(
        Event::Candidates {
            ticket,
            result: Err(Refusal::code("callosum unreachable · retrying")),
        },
        &Devices::default(),
    );
    flow.key(KeyCode::Enter, &Devices::default());
    assert!(flow.outbox.is_empty() && flow.list_failed);
    flow.tick(13_000);
    let Command::Candidates { ticket } = flow.outbox.remove(0) else {
        panic!()
    };
    flow.absorb(
        Event::Candidates {
            ticket,
            result: computer::candidates(&sample("candidates_synced")["reply"]),
        },
        &Devices::default(),
    );
    flow.key(KeyCode::Enter, &Devices::default());
    assert!(matches!(flow.outbox[0], Command::Open { .. }));
}

#[test]
fn app_ceremony_keys_are_modal_and_only_plain_y_confirms() {
    use ck_ui::app::{Request, Tab};
    use ck_ui::{App, Snapshot, Theme};
    use crossterm::event::{KeyEvent, KeyModifiers};
    let snap = Snapshot::fixture();
    let mut app = App::new(Theme::test());
    app.tab = Tab::Devices;
    app.computer = opened_flow();
    app.computer.absorb(
        Event::Watch {
            window_id: 4294967297,
            state: computer::Watch::Code {
                code: "082917".into(),
                confirm_by_ms: 4_000_000_120_000,
            },
        },
        &Devices::default(),
    );
    for code in [
        KeyCode::Enter,
        KeyCode::Char('q'),
        KeyCode::Char('1'),
        KeyCode::Char('a'),
    ] {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE), &snap);
    }
    assert!(!app.quit && app.tab == Tab::Devices && app.overlay.is_none());
    assert!(app.requests.is_empty());
    app.handle_key(
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
        &snap,
    );
    assert!(app.requests.is_empty());
    app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &snap);
    assert_eq!(
        app.requests,
        vec![Request::Computer(Command::Confirm {
            window_id: 4294967297,
            matches: true
        })]
    );
    app.requests.clear();
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &snap);
    assert_eq!(
        app.requests,
        vec![
            Request::Computer(Command::Cancel {
                window_id: 4294967297
            }),
            Request::Computer(Command::Unwatch {
                window_id: 4294967297
            })
        ]
    );
}

#[test]
fn partial_forget_remains_retryable_after_inventory_removes_the_entry() {
    use ck_ui::{App, Snapshot, Theme};
    let mut snap = Snapshot::fixture();
    snap.devices.data = shared_devices();
    snap.devices.data.list[0].name = "Studio PC".into();
    let id = snap
        .devices
        .data
        .inventory
        .computers
        .keys()
        .next()
        .unwrap()
        .clone();
    let mut app = App::new(Theme::test());
    app.tab = ck_ui::app::Tab::Devices;
    app.computer.detail = Some(id);
    app.computer.forget = true;
    app.computer.key(KeyCode::Char('x'), &snap.devices.data);
    let Command::Forget { ticket, .. } = app.computer.outbox.remove(0) else {
        panic!()
    };
    app.computer.absorb(
        Event::Forgotten {
            ticket,
            result: Err(Refusal::code("fed_target_unavailable")),
        },
        &snap.devices.data,
    );
    snap.devices.data = Devices::default();
    app.on_snapshot(&snap, Snapshot::fixture_now());
    let mut buffer = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 120, 36));
    app.render(&mut buffer, &snap, Snapshot::fixture_now());
    let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
    assert!(text.contains("is only partly forgotten · x tries again"));
    app.computer.key(KeyCode::Char('x'), &snap.devices.data);
    let Command::Forget { ticket, .. } = app.computer.outbox.remove(0) else {
        panic!()
    };
    app.computer.absorb(
        Event::Forgotten {
            ticket,
            result: Err(Refusal::code("fed_not_paired")),
        },
        &snap.devices.data,
    );
    assert_eq!(app.computer.notice.as_deref(), Some("✓ Already forgotten. Studio PC still lists this computer until you forget it there too."));
}
