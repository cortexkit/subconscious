//! Executes named-computer operations through the same interface used for phone pairing.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use crate::computer::{self, Command, Event};
use crate::live::pairing::{CallFailure, Callosum};
use crate::pairing::flow::Refusal;

pub struct Computer {
    callosum: Arc<dyn Callosum>,
    sink: Arc<dyn Fn(Event) + Send + Sync>,
    watches: Mutex<HashMap<u64, tokio::sync::oneshot::Sender<()>>>,
}

impl Computer {
    pub fn new(callosum: Arc<dyn Callosum>, sink: Arc<dyn Fn(Event) + Send + Sync>) -> Arc<Self> {
        Arc::new(Self {
            callosum,
            sink,
            watches: Mutex::new(HashMap::new()),
        })
    }

    pub fn run(self: &Arc<Self>, command: Command) {
        if let Command::Watch { window_id } = command {
            self.watch(window_id);
            return;
        }
        if let Command::Unwatch { window_id } = command {
            self.watches
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&window_id);
            return;
        }
        let Some((method, params)) = command.request() else {
            return;
        };
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let timeout = if matches!(command, Command::Confirm { .. } | Command::Sharing { .. }) {
                Duration::from_millis(crate::pairing::flow::CONFIRM_TIMEOUT_MS)
            } else {
                Duration::from_secs(10)
            };
            let result = this.callosum.call(method, params, timeout).await;
            let result = reply(result);
            let event = match command {
                Command::Candidates { ticket } => Event::Candidates {
                    ticket,
                    result: result.and_then(|v| computer::candidates(&v)),
                },
                Command::Forget { ticket, .. } => Event::Forgotten {
                    ticket,
                    result: result.and_then(|v| computer::empty_reply(&v)),
                },
                Command::ReadDevices => return,
                Command::Open { ticket, .. } => Event::Opened {
                    ticket,
                    result: result.and_then(|v| computer::opened(&v)),
                },
                Command::Confirm { window_id, matches } => Event::Confirmed {
                    window_id,
                    matches,
                    result: result.and_then(|v| computer::empty_reply(&v)),
                },
                Command::Sharing { ticket, .. } => Event::Shared {
                    ticket,
                    result: result.and_then(|v| computer::empty_reply(&v)),
                },
                Command::Cancel { .. } | Command::Watch { .. } | Command::Unwatch { .. } => return,
            };
            (this.sink)(event);
        });
    }

    fn watch(self: &Arc<Self>, window_id: u64) {
        let (tx, mut stop) = tokio::sync::oneshot::channel();
        {
            let mut watches = self.watches.lock().unwrap_or_else(|e| e.into_inner());
            if watches.contains_key(&window_id) {
                return;
            }
            watches.insert(window_id, tx);
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut stream = tokio::select! {
                result = this.callosum.watch_computer(window_id) => match result {
                    Ok(stream) => stream,
                    Err(_) => {
                        this.watches.lock().unwrap_or_else(|e| e.into_inner()).remove(&window_id);
                        (this.sink)(Event::WatchLost { window_id });
                        return;
                    }
                },
                _ = &mut stop => return,
            };
            loop {
                let next = tokio::select! {
                    next = stream.next() => next,
                    _ = &mut stop => { stream.close().await; return; }
                };
                let state = next.as_ref().and_then(|v| computer::watch(v).ok());
                match state {
                    Some(state) => {
                        let terminal = state.terminal();
                        (this.sink)(Event::Watch { window_id, state });
                        if !terminal {
                            continue;
                        }
                    }
                    None => {
                        (this.sink)(Event::WatchLost { window_id });
                    }
                }
                this.watches
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&window_id);
                stream.close().await;
                return;
            }
        });
    }
}

pub fn reply(result: Result<serde_json::Value, CallFailure>) -> Result<serde_json::Value, Refusal> {
    match result {
        Ok(value) => match super::pairing::reply_refusal(&value) {
            Some(e) => Err(e),
            None => Ok(value),
        },
        Err(CallFailure::Refused(e)) => Err(e),
        Err(CallFailure::Lost) => Err(Refusal::code("callosum unreachable · retrying")),
    }
}
