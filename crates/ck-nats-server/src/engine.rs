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
    process::Command,
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

impl Engine {
    pub async fn spawn(
        executable: &Path,
        config: &Path,
        snapshot: Arc<RwLock<Snapshot>>,
        bus: Arc<RwLock<Option<BusRoute>>>,
    ) -> Result<(Self, JoinHandle<Result<(), String>>), String> {
        let mut command = Command::new(executable);
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("SUBC_") {
                command.env_remove(name);
            }
        }
        let mut child = command
            .arg("-c")
            .arg(config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("start engine: {e}"))?;
        let mut input = child.stdin.take();
        let output = child.stdout.take().ok_or("engine stdout unavailable")?;
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
                let deadline = Instant::now() + budget;
                let frame = serde_json::json!({"type":"shutdown","deadline_ms":budget.as_millis().clamp(1,20_000) as u64});
                match timeout(budget, wire::send(input.as_mut().unwrap(), frame)).await {
                    Ok(Ok(())) => {},
                    other => eprintln!("ck-nats-server: shutdown write failed: {other:?}"),
                }
                // Closing stdin is the fallback even when an engine stops reading.
                drop(input.take());
                match timeout(deadline.saturating_duration_since(Instant::now()), child.wait()).await {
                    Ok(Ok(exit)) if exit.success() => {},
                    other => eprintln!("ck-nats-server: engine stop: {other:?}"),
                }
                Ok(())
              }
              exit = child.wait() => Err(format!("engine exit: {exit:?}")),
              fatal = fatal_rx => Err(fatal.unwrap_or_else(|e| format!("engine reader stopped: {e}"))),
              result = async {
                wire::send(input.as_mut().unwrap(), state_frame(0, vec![])).await.map_err(|e| e.to_string())?;
                loop {
                    tokio::select! {
                        command = receive.recv() => match command {
                            Some(frame) => wire::send(input.as_mut().unwrap(), frame).await.map_err(|e| e.to_string())?,
                            None => return Err("engine command channel closed".into()),
                        },
                        message = incoming.recv() => match message {
                            Some(EngineMessage::Ready {version,nats_version,patch}) => {
                                eprintln!("ck-nats-server: engine {version}, nats {nats_version}, patch {patch} ready");
                                snapshot.write().await.ready = true;
                            }
                            Some(EngineMessage::Status {jetstream,leafs,listener}) => snapshot.write().await.status = Some((jetstream,leafs,listener)),
                            Some(EngineMessage::Applied {generation}) => {
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
            drop(input);
            signing.abort_all();
            while signing.join_next().await.is_some() {}
            reading.abort();
            let _ = reading.await;
            if result.is_err() {
                // EOF, not a signal, asks the engine to stop on every failure path.
                let _ = timeout(Duration::from_secs(5), child.wait()).await;
            }
            let _ = done_tx.send(true);
            result
        });
        Ok((handle, running))
    }

    pub async fn state(&self, generation: u64, remotes: Vec<Value>) -> Result<(), String> {
        self.commands
            .send(state_frame(generation, remotes))
            .await
            .map_err(|e| e.to_string())
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
