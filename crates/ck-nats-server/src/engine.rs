use std::{
    path::Path,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use serde_json::Value;
use tokio::{
    io::BufReader,
    process::{Child, ChildStdin, Command},
    sync::{mpsc, oneshot, watch, RwLock},
    task::{JoinHandle, JoinSet},
    time::{timeout, Instant},
};

use crate::{
    bus::{self, BusRoute},
    state_frame,
    wire::{self, EngineMessage, EngineReader},
    Snapshot,
};

#[derive(Clone)]
pub(crate) struct Engine {
    commands: mpsc::Sender<Value>,
    stop: mpsc::Sender<Duration>,
    stopping: Arc<AtomicBool>,
    done: watch::Receiver<bool>,
}

fn spawn_child(executable: &Path, config: &Path, hub: bool) -> Result<Child, String> {
    let mut command = Command::new(executable);
    if hub {
        command.arg("--hub");
    }
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("SUBC_") {
            command.env_remove(name);
        }
    }
    command
        .arg("-c")
        .arg(config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("start engine: {e}"))
}

async fn stop_child(
    child: &mut Child,
    input: &mut Option<ChildStdin>,
    budget: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + budget;
    let frame = serde_json::json!({"type":"shutdown","deadline_ms":budget.as_millis().clamp(1,20_000) as u64});
    match timeout(budget, wire::send(input.as_mut().unwrap(), frame)).await {
        Ok(Ok(())) => {}
        other => eprintln!("ck-nats-server: shutdown write failed: {other:?}"),
    }
    // EOF is the fallback if the child stopped reading shutdown commands.
    drop(input.take());
    match timeout(
        deadline.saturating_duration_since(Instant::now()),
        child.wait(),
    )
    .await
    {
        Ok(Ok(exit)) if exit.success() => Ok(()),
        other => Err(format!("engine stop: {other:?}")),
    }
}

impl Engine {
    pub async fn spawn(
        executable: &Path,
        config: &Path,
        snapshot: Arc<RwLock<Snapshot>>,
        bus: Arc<RwLock<Option<BusRoute>>>,
        hub: bool,
    ) -> Result<(Self, JoinHandle<Result<(), String>>), String> {
        let mut child = spawn_child(executable, config, hub)?;
        let executable = executable.to_owned();
        let config = config.to_owned();
        let (commands, mut receive) = mpsc::channel(16);
        let (stop, mut stop_rx) = mpsc::channel(1);
        let (done_tx, done) = watch::channel(false);
        let stopping = Arc::new(AtomicBool::new(false));
        let handle = Self {
            commands,
            stop,
            stopping,
            done,
        };
        let running = tokio::spawn(async move {
            let mut initial = state_frame(0, vec![]);
            let result = loop {
                let mut input = child.stdin.take();
                let Some(output) = child.stdout.take() else {
                    break Err("engine stdout unavailable".into());
                };
                let mut listener = initial.get("hub_listener").cloned();
                let mut latest_generation = initial["generation"].as_u64().unwrap();
                let mut applied_generation = None;
                let (messages, mut incoming) = mpsc::channel(16);
                let (fatal_tx, fatal_rx) = oneshot::channel();
                let reading = tokio::spawn(async move {
                    let mut reader = EngineReader::new(BufReader::new(output));
                    loop {
                        match reader.next().await {
                            Ok(Some(message)) => {
                                if messages.send(message).await.is_err() {
                                    break;
                                }
                            }
                            end => {
                                let _ = fatal_tx.send(format!("engine stdout ended: {end:?}"));
                                break;
                            }
                        }
                    }
                });
                let mut signing = JoinSet::new();
                let result = tokio::select! {
                  biased;
                  Some(budget) = stop_rx.recv() => {
                    if let Err(error) = stop_child(&mut child, &mut input, budget).await {
                        eprintln!("ck-nats-server: {error}");
                    }
                    Ok(None)
                  }
                  exit = child.wait() => Err(format!("engine exit: {exit:?}")),
                  fatal = fatal_rx => Err(fatal.unwrap_or_else(|e| format!("engine reader stopped: {e}"))),
                  result = async {
                    wire::send(input.as_mut().unwrap(), initial.clone()).await.map_err(|e| e.to_string())?;
                    loop {
                        tokio::select! {
                            command = receive.recv() => match command {
                                Some(frame) => {
                                    let generation = frame["generation"].as_u64().unwrap();
                                    if hub && generation >= latest_generation && !applied_generation.is_some_and(|g| generation <= g) {
                                        let next_listener = frame.get("hub_listener").cloned();
                                        // NATS cannot replace or remove its listener by reload. Retire
                                        // the old child before giving the watch state to a fresh one.
                                        if listener.is_some() && next_listener != listener {
                                            return Ok(Some(frame));
                                        }
                                        snapshot.write().await.hub_listener_requested = next_listener.is_some();
                                        listener = next_listener;
                                        latest_generation = generation;
                                    }
                                    wire::send(input.as_mut().unwrap(), frame).await.map_err(|e| e.to_string())?;
                                },
                                None => return Err("engine command channel closed".into()),
                            },
                            message = incoming.recv() => match message {
                                Some(EngineMessage::Ready {version,nats_version,patch}) => {
                                    eprintln!("ck-nats-server: engine {version}, nats {nats_version}, patch {patch} ready");
                                    snapshot.write().await.ready = true;
                                }
                                Some(EngineMessage::Status {jetstream,leafs,listener}) => snapshot.write().await.status = Some((jetstream,leafs,listener)),
                                Some(EngineMessage::Applied {generation}) => {
                                    applied_generation = Some(applied_generation.unwrap_or(0).max(generation));
                                    eprintln!("ck-nats-server: applied generation {generation}");
                                    snapshot.write().await.apply_failure = None;
                                }
                                Some(EngineMessage::ApplyFailed {generation,error}) => snapshot.write().await.apply_failure = Some((generation,error)),
                                Some(request @ (EngineMessage::Sign {..} | EngineMessage::TlsSign {..})) => {
                                    let bus = bus.clone();
                                    signing.spawn(async move { bus::sign(bus, request).await });
                                }
                                _ => return Err("engine stdout EOF".into()),
                            },
                            Some(reply) = signing.join_next() => {
                                let reply = reply.map_err(|e| e.to_string())?;
                                wire::send(input.as_mut().unwrap(), reply).await.map_err(|e| e.to_string())?;
                            }
                        }
                    }
                  } => result,
                };
                signing.abort_all();
                while signing.join_next().await.is_some() {}
                reading.abort();
                let _ = reading.await;
                match result {
                    Ok(Some(frame)) => {
                        *snapshot.write().await = Snapshot {
                            hub_listener_requested: frame.get("hub_listener").is_some(),
                            ..Snapshot::default()
                        };
                        if let Err(error) =
                            stop_child(&mut child, &mut input, Duration::from_secs(20)).await
                        {
                            break Err(error);
                        }
                        initial = frame;
                        match spawn_child(&executable, &config, hub) {
                            Ok(next) => child = next,
                            Err(error) => break Err(error),
                        }
                    }
                    Ok(None) => break Ok(()),
                    Err(error) => {
                        drop(input);
                        // EOF, not a signal, asks the engine to stop on every failure path.
                        let _ = timeout(Duration::from_secs(5), child.wait()).await;
                        break Err(error);
                    }
                }
            };
            let _ = done_tx.send(true);
            result
        });
        Ok((handle, running))
    }

    pub async fn state(
        &self,
        generation: u64,
        remotes: Vec<Value>,
        listener: Option<bus::HubListener>,
    ) -> Result<(), String> {
        let mut frame = state_frame(generation, remotes);
        if let Some(listener) = listener {
            frame["hub_listener"] = serde_json::to_value(listener).map_err(|e| e.to_string())?;
        }
        self.commands.send(frame).await.map_err(|e| e.to_string())
    }

    pub fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    pub async fn stop(&self, budget: Duration) {
        if !self.stopping.swap(true, Ordering::SeqCst) {
            let budget = budget.clamp(Duration::from_millis(1), Duration::from_secs(20));
            let _ = self.stop.send(budget).await;
        }
        let mut done = self.done.clone();
        while !*done.borrow_and_update() {
            if done.changed().await.is_err() {
                break;
            }
        }
    }
}
