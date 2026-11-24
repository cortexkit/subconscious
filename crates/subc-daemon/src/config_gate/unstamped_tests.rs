use super::*;
use crate::router::test_log::{captured_logs, log_capture};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

const UNDECLARED: ConnectionId = ConnectionId::new(30);

fn write_limit(rig: &Rig, limit: u64) {
    std::fs::write(
        rig.path.join("cortexkit/a.jsonc"),
        format!("{{\"limit\":{limit}}}"),
    )
    .unwrap();
}

struct LiveFiles {
    rig: Rig,
    owner: Arc<RuntimeOwner>,
    registry: Arc<crate::Registry>,
    forwarding: Arc<crate::ForwardingTable>,
    _handler: Arc<crate::ControlHandler>,
    _worker: WorkerGuard,
    _clock: WorkerGuard,
    idle: mpsc::UnboundedReceiver<(ConnectionId, oneshot::Sender<()>)>,
    paused: HashMap<ConnectionId, oneshot::Sender<()>>,
    receivers: Vec<(ConnectionId, mpsc::Receiver<crate::router::OutboundFrame>)>,
    pushes: Vec<(ConnectionId, serde_json::Value)>,
}

impl LiveFiles {
    async fn new() -> Self {
        let clock = hold_test_clock();
        let mut rig = Rig::new();
        // An absent user file is valid, so appearance and removal can be observed.
        rig.declaration.schema["properties"]
            .as_object_mut()
            .unwrap()
            .remove("credential");
        write_limit(&rig, 1);
        let owner = Arc::new(rig.owner("live-files"));
        rig.declare(&owner, INCUMBENT.get());
        let registry = Arc::new(crate::Registry::default());
        let forwarding = Arc::new(crate::ForwardingTable::default());
        let handler = Arc::new(
            crate::ControlHandler::with_forwarding(registry.clone(), forwarding.clone())
                .with_config_runtime(owner.clone()),
        );
        let (sender, idle) = mpsc::unbounded_channel();
        *owner.user_file_test_idle.lock().unwrap() = Some(sender);
        let worker = WorkerGuard(handler.spawn_config_delivery_worker().unwrap());
        let mut live = Self {
            rig,
            owner,
            registry,
            forwarding,
            _handler: handler,
            _worker: worker,
            _clock: clock,
            idle,
            paused: HashMap::new(),
            receivers: vec![],
            pushes: vec![],
        };
        live.register(INCUMBENT, "a");
        live.get();
        live.owner.notify_delivery_changed();
        live.until_idle(INCUMBENT).await;
        assert!(
            live.pushes.is_empty(),
            "an initial observation is not an edit"
        );
        live
    }

    fn register(&mut self, connection: ConnectionId, module: &str) {
        self.registry
            .register_with_control_ops(
                subc_protocol::manifest::ModuleManifest::builder(module, "0.1.0").build(),
                subc_protocol::PROTOCOL_VERSION,
                connection,
                vec!["config.review".into()],
            )
            .unwrap();
        let (sender, receiver) = mpsc::channel(8);
        self.forwarding
            .register_module_connection(
                connection,
                module.into(),
                subc_protocol::PROTOCOL_VERSION,
                subc_protocol::manifest::Concurrency::ModuleManaged,
                crate::FrameSink::new(sender),
            )
            .unwrap();
        self.receivers.push((connection, receiver));
    }

    fn get(&self) -> subc_protocol::config::ConfigGetReply {
        let reply = self
            .owner
            .module_get(INCUMBENT, "a", ConfigGetRequest::new())
            .unwrap();
        self.owner
            .module_view_delivered(INCUMBENT, "a", &reply, tokio::time::Instant::now(), true)
            .unwrap();
        reply
    }

    async fn until_idle(&mut self, connection: ConnectionId) {
        wait_for_blocking_delivery(|| {
            for (connection, receiver) in &mut self.receivers {
                while let Ok(mut outbound) = receiver.try_recv() {
                    assert_eq!(outbound.frame.header.ty, subc_protocol::FrameType::Push);
                    let body: serde_json::Value =
                        serde_json::from_slice(&outbound.frame.body).unwrap();
                    assert_eq!(body["op"], "config.changed");
                    self.pushes.push((*connection, body));
                    outbound.flushed.take().unwrap().send(()).unwrap();
                }
            }
            while let Ok((connection, resume)) = self.idle.try_recv() {
                assert!(self.paused.insert(connection, resume).is_none());
            }
            self.paused.contains_key(&connection)
        })
        .await;
    }

    async fn tick(&mut self) {
        // The worker is held just before select, with its two-second deadline set.
        tokio::time::advance(Duration::from_secs(2)).await;
        self.paused.remove(&INCUMBENT).unwrap().send(()).unwrap();
        self.until_idle(INCUMBENT).await;
    }

    fn expect_edit(&mut self) {
        assert_eq!(
            self.pushes.len(),
            1,
            "one generation-0 edit hint: {:?}",
            self.pushes
        );
        assert_eq!(self.pushes[0].0, INCUMBENT);
        assert_eq!(self.pushes[0].1["generation"], 0);
        self.pushes.clear();
    }

    async fn real_edit(&mut self) {
        write_limit(&self.rig, 2);
        self.tick().await;
        self.expect_edit();
        assert_eq!(self.get().active.unwrap().effective["limit"], 2);
    }
}

#[tokio::test(start_paused = true)]
async fn config_gate_unstamped_user_file_edit_pushes_and_get_returns_new_bytes() {
    let mut live = LiveFiles::new().await;
    live.real_edit().await;
    let path = live.rig.path.join("cortexkit/a.jsonc");
    let raw = b"{ /* literal user bytes */ \"limit\":3 }";
    std::fs::write(&path, raw).unwrap();
    live.tick().await;
    live.expect_edit();
    let reply = live.get();
    let view = reply.active.unwrap();
    assert_eq!(view.generation, 0);
    assert_eq!(view.status, ConfigStatus::Unstamped);
    assert_eq!(view.layers[0].bytes.as_deref(), Some(raw.as_slice()));
    assert!(reply.pending.is_none());

    std::fs::remove_file(&path).unwrap();
    live.tick().await;
    live.expect_edit();
    assert_eq!(live.get().active.unwrap().layers[0].bytes, None);
    std::fs::write(&path, raw).unwrap();
    live.tick().await;
    live.expect_edit();
    assert_eq!(
        live.get().active.unwrap().layers[0].bytes.as_deref(),
        Some(raw.as_slice())
    );

    live.owner.disconnected(INCUMBENT);
    let mut watch = crate::config_gate::runtime::unstamped::UserFileWatch::default();
    assert_eq!(
        live.owner
            .poll_user_file(INCUMBENT, "a", &mut watch)
            .unwrap(),
        crate::config_gate::runtime::unstamped::UserFilePoll::Stopped
    );
    assert!(!live
        .owner
        .lock()
        .unwrap()
        .delivery
        .unstamped_views
        .contains_key(&INCUMBENT));
}

#[tokio::test(start_paused = true)]
async fn config_gate_unstamped_user_file_same_bytes_and_touch_send_no_push() {
    let mut live = LiveFiles::new().await;
    live.real_edit().await;
    let path = live.rig.path.join("cortexkit/a.jsonc");
    let file = std::fs::File::options().write(true).open(&path).unwrap();
    let modified = file.metadata().unwrap().modified().unwrap() + Duration::from_secs(10);
    file.set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    live.tick().await;
    assert!(
        live.pushes.is_empty(),
        "a touch must not push: {:?}",
        live.pushes
    );

    let replacement = live.rig.path.join("same-bytes.jsonc");
    std::fs::write(&replacement, std::fs::read(&path).unwrap()).unwrap();
    std::fs::rename(replacement, path).unwrap();
    live.tick().await;
    assert!(
        live.pushes.is_empty(),
        "an identical replacement must not push: {:?}",
        live.pushes
    );
}

#[test]
fn config_gate_unstamped_user_file_refusal_logs_once_and_reports_path() {
    use crate::config_gate::runtime::unstamped::UserFileWatch;

    let mut rig = Rig::new();
    rig.declaration.schema["properties"]
        .as_object_mut()
        .unwrap()
        .remove("credential");
    write_limit(&rig, 1);
    let owner = rig.owner("user-file-refusal");
    rig.declare(&owner, INCUMBENT.get());
    let good = owner
        .module_get(INCUMBENT, "a", ConfigGetRequest::new())
        .unwrap();
    owner
        .module_view_delivered(INCUMBENT, "a", &good, tokio::time::Instant::now(), true)
        .unwrap();

    let (logs, _guard) = log_capture(tracing::Level::INFO);
    let mut watch = UserFileWatch::default();
    let path = &rig.declaration.user_file;
    let refused = br#"{"limit":2,"not/declared":true}"#;
    std::fs::write(path, refused).unwrap();
    for _ in 0..3 {
        // Run the same blocking poll as the delivery worker on this thread so
        // the scoped tracing subscriber can see its events.
        let _ = owner.poll_user_file(INCUMBENT, "a", &mut watch);
        assert!(!owner.take_unstamp_hint(INCUMBENT).unwrap());
    }
    let text = captured_logs(&logs);
    let warnings: Vec<_> = text
        .lines()
        .filter(|line| line.contains("user settings file refused"))
        .collect();
    assert_eq!(warnings.len(), 1, "{text}");
    for part in ["WARN", "module_id=a", path, "reason=a: /not~1declared"] {
        assert!(warnings[0].contains(part), "{}", warnings[0]);
    }
    assert_eq!(
        serde_json::to_value(owner.polled_status().unwrap()).unwrap()["file_errors"],
        json!([{
            "module_id":"a", "code":"value_invalid", "message":"a: /not~1declared", "path":path
        }])
    );
    assert_eq!(
        owner.lock().unwrap().delivery.unstamped_views[&INCUMBENT].as_ref(),
        good.active.as_ref().unwrap()
    );

    let replacement = rig.path.join("replacement.jsonc");
    std::fs::write(&replacement, refused).unwrap();
    std::fs::rename(replacement, path).unwrap();
    // A fresh worker after reconnect must not log the same bytes again.
    let mut watch = UserFileWatch::default();
    owner.poll_user_file(INCUMBENT, "a", &mut watch).unwrap();
    assert_eq!(
        captured_logs(&logs)
            .matches("user settings file refused")
            .count(),
        1
    );

    std::fs::write(path, br#"{"limit":3,"not/declared":true}"#).unwrap();
    owner.poll_user_file(INCUMBENT, "a", &mut watch).unwrap();
    assert_eq!(
        captured_logs(&logs)
            .matches("user settings file refused")
            .count(),
        2
    );
    assert!(!owner.take_unstamp_hint(INCUMBENT).unwrap());

    write_limit(&rig, 3);
    owner.poll_user_file(INCUMBENT, "a", &mut watch).unwrap();
    assert!(owner.take_unstamp_hint(INCUMBENT).unwrap());
    owner.poll_user_file(INCUMBENT, "a", &mut watch).unwrap();
    assert!(owner.polled_status().unwrap().file_errors.is_empty());
    let text = captured_logs(&logs);
    let recoveries: Vec<_> = text
        .lines()
        .filter(|line| line.contains("user settings file accepted after refusal"))
        .collect();
    assert_eq!(recoveries.len(), 1, "{text}");
    for part in ["INFO", "module_id=a", path] {
        assert!(recoveries[0].contains(part), "{}", recoveries[0]);
    }
}

#[tokio::test(start_paused = true)]
async fn config_gate_unstamped_user_file_stamp_stops_generation_zero_pushes() {
    let mut live = LiveFiles::new().await;
    live.real_edit().await;
    let generation = live.rig.stamp(&live.owner);
    // Finish adopting the approved stamp so its stamped state, rather than an
    // unfinished configuration transition, is what stops user-file polling.
    live.owner
        .complete_authority_exit_for_test(generation)
        .unwrap();
    write_limit(&live.rig, 3);
    live.tick().await;
    assert_eq!(
        live.pushes.len(),
        1,
        "the approved pending generation must still be delivered"
    );
    assert!(
        live.pushes
            .iter()
            .all(|(_, push)| push["generation"] == generation),
        "{:?}",
        live.pushes
    );
    let mut watch = crate::config_gate::runtime::unstamped::UserFileWatch::default();
    assert_eq!(
        live.owner
            .poll_user_file(INCUMBENT, "a", &mut watch)
            .unwrap(),
        crate::config_gate::runtime::unstamped::UserFilePoll::Stopped,
        "stamped files must not be watched"
    );

    let prepared = live.owner.prepare(PlanKind::Unstamp).unwrap();
    let plan = live.owner.finish_plan(prepared, daemon_review).unwrap();
    live.owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let challenge = live
        .owner
        .begin_approval(&plan.digest, &Rig::facts())
        .unwrap();
    let unstamped = live
        .owner
        .finish_approval(
            challenge.attempt(),
            &Rig::facts(),
            ApprovalReply::Presence(crate::config_gate::PresenceDecision::InteractiveApproval),
            2,
        )
        .unwrap();
    // A real daemon clears the authority-exit fence after reconciling supervised
    // processes. This fixture has no processes, so clear the fence before polling resumes.
    live.owner
        .complete_authority_exit_for_test(unstamped)
        .unwrap();
    live.pushes.clear();
    live.tick().await;
    assert!(live.pushes.iter().all(|(_, push)| push["generation"] == 0));
    live.pushes.clear();
    assert_eq!(live.get().active.unwrap().effective["limit"], 3);
    write_limit(&live.rig, 4);
    live.tick().await;
    live.expect_edit();
    assert_eq!(live.get().active.unwrap().effective["limit"], 4);
}

#[tokio::test(start_paused = true)]
async fn config_gate_unstamped_user_file_undeclared_module_gets_no_push() {
    let mut live = LiveFiles::new().await;
    live.real_edit().await;
    live.register(UNDECLARED, "c");
    std::fs::write(live.rig.path.join("cortexkit/c.jsonc"), b"{}").unwrap();
    live.owner.notify_delivery_changed();
    live.until_idle(UNDECLARED).await;
    assert!(
        live.pushes.is_empty(),
        "an opted-in but undeclared module must not get a push: {:?}",
        live.pushes
    );
    let mut watch = crate::config_gate::runtime::unstamped::UserFileWatch::default();
    assert_eq!(
        live.owner
            .poll_user_file(UNDECLARED, "c", &mut watch)
            .unwrap(),
        crate::config_gate::runtime::unstamped::UserFilePoll::Stopped
    );
}

#[tokio::test(start_paused = true)]
async fn config_gate_unstamped_user_file_over_bound_is_never_unchanged() {
    let live = LiveFiles::new().await;
    std::fs::write(
        live.rig.path.join("cortexkit/a.jsonc"),
        vec![b' '; 1024 * 1024 + 1],
    )
    .unwrap();
    let mut watch = crate::config_gate::runtime::unstamped::UserFileWatch::default();
    for _ in 0..2 {
        assert_eq!(
            live.owner
                .poll_user_file(INCUMBENT, "a", &mut watch)
                .unwrap(),
            crate::config_gate::runtime::unstamped::UserFilePoll::Observed
        );
    }
}

#[tokio::test(start_paused = true)]
async fn config_gate_unstamped_user_file_unchanged_tick_takes_no_owner_guard() {
    use std::sync::atomic::Ordering::SeqCst;
    let mut live = LiveFiles::new().await;
    live.real_edit().await;
    // Let the edit's notifications settle before counting.
    live.tick().await;
    live.tick().await;
    let guards = live.owner.owner_guards.load(SeqCst);
    let builds = live.owner.projection_builds.load(SeqCst);
    for _ in 0..3 {
        live.tick().await;
    }
    assert!(live.pushes.is_empty(), "{:?}", live.pushes);
    assert_eq!(
        live.owner.owner_guards.load(SeqCst),
        guards,
        "an unchanged timer pass must not take an owner guard"
    );
    assert_eq!(live.owner.projection_builds.load(SeqCst), builds);

    // A change on the next tick still takes the full pass and pushes.
    let modified = std::fs::metadata(live.rig.path.join("cortexkit/a.jsonc"))
        .unwrap()
        .modified()
        .unwrap();
    write_limit(&live.rig, 5);
    {
        // Filesystems can give two same-size writes the same modification time.
        // Keep that time identical even if this test runs across a clock tick.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(live.rig.path.join("cortexkit/a.jsonc"))
            .unwrap();
        file.set_modified(modified).unwrap();
        assert_eq!(file.metadata().unwrap().modified().unwrap(), modified);
    }
    live.tick().await;
    live.expect_edit();
    assert!(live.owner.owner_guards.load(SeqCst) > guards);
}
