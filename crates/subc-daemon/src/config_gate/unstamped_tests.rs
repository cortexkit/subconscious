use super::*;
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
    assert!(!live
        .owner
        .poll_unstamped_user_file(INCUMBENT, "a", &mut watch)
        .unwrap());
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
    assert!(
        !live
            .owner
            .poll_unstamped_user_file(INCUMBENT, "a", &mut watch)
            .unwrap(),
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
    assert!(!live
        .owner
        .poll_unstamped_user_file(UNDECLARED, "c", &mut watch)
        .unwrap());
}
