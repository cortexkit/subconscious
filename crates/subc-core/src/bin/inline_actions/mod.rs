//! Terminal lifecycle progress and optional plain summaries of observed outcomes.

use std::future::Future;
use std::pin::Pin;

use ck_ui::live::actions::{self, ActionSource, Reading};
use ck_ui::live::{LiveProcess, State};
use ck_ui::model::ActionKind;
use ck_ui::time::Timestamp;

use super::*;
use std::io::Write;

pub(super) fn set_wait(wait: &mut Option<bool>, value: bool) -> Result<(), CkError> {
    if wait.is_some_and(|old| old != value) {
        return Err(CkError::Usage(
            "pass either --wait or --no-wait, not both".into(),
        ));
    }
    *wait = Some(value);
    Ok(())
}

pub(super) async fn run(
    client: &mut CkClient,
    kind: ActionKind,
    module: &str,
    drain_timeout_ms: Option<u64>,
    inline: bool,
) -> Result<(), CkError> {
    let source = CliAction {
        path: client.path.clone(),
        client: tokio::sync::Mutex::new(client),
        module: module.into(),
        gauges: std::sync::Mutex::new(None),
        inline,
    };
    let mut line = String::new();
    let following = actions::run_action(
        &source,
        kind,
        module,
        drain_timeout_ms,
        true,
        |_phase, text| {
            if inline && text != line {
                print!(
                    "\r\x1b[2K{}",
                    ck_ui::draw::trunc(
                        &text,
                        terminal_size::terminal_size().map_or(100, |(w, _)| w.0 as usize)
                    )
                );
                let _ = io::stdout().flush();
                line = text;
            }
        },
    );
    let outcome = tokio::select! {
        result = following => result,
        _ = tokio::signal::ctrl_c() => {
            if inline { print!("\r\x1b[2K"); }
            println!("Stopped watching; the action was not cancelled. Check it with ck module list.");
            return Ok(());
        }
    };
    if inline {
        print!("\r\x1b[2K");
    }
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            if inline {
                println!("✗ {module}: {error}");
            }
            return Err(error);
        }
    };
    println!(
        "{}",
        if inline {
            outcome.inline
        } else {
            outcome.plain
        }
    );
    if outcome.ok {
        Ok(())
    } else {
        Err(CkError::RenderedExit { exit_code: 1 })
    }
}

struct CliAction<'a> {
    path: PathBuf,
    client: tokio::sync::Mutex<&'a mut CkClient>,
    module: String,
    gauges: std::sync::Mutex<Option<Vec<String>>>,
    inline: bool,
}

impl ActionSource for CliAction<'_> {
    type Error = CkError;

    fn read(&self) -> Pin<Box<dyn Future<Output = Result<Reading, CkError>> + Send + '_>> {
        Box::pin(async move {
            let mut client = self.client.lock().await;
            let gauges = self.gauges.lock().unwrap().clone();
            let gauges = if let Some(gauges) = gauges {
                gauges
            } else {
                let gauges = actions::busy_gauges(&client.catalog_list().await?, &self.module);
                *self.gauges.lock().unwrap() = Some(gauges.clone());
                gauges
            };
            let mut state = State::empty("This computer");
            // Cached health has no pid. Bracket it with the process census and
            // reject a check older than the process whose gauges it would name.
            let before = spawns(&mut client).await?;
            let roster = client
                .rpc_value(ClientControlRequest::SupervisorList {})
                .await?;
            if let ClientControlResponse::SupervisorList { modules, .. } =
                serde_json::from_value(roster)?
            {
                state.record_roster(modules, Timestamp::system_now());
            }
            let health = client
                .rpc_value(ClientControlRequest::SupervisorHealth {})
                .await?;
            if let ClientControlResponse::SupervisorHealth { modules, .. } =
                serde_json::from_value(health)?
            {
                state.health.set(modules, Timestamp::system_now());
            }
            let after = spawns(&mut client).await?;
            let stable = before.get(&self.module) == after.get(&self.module)
                && before.get(&self.module).is_some_and(|p| {
                    state
                        .health_of(&self.module)
                        .and_then(|h| h.last_probe_ms)
                        .is_some_and(|ms| ms as i64 >= p.spawned_at.unix_millis())
                });
            state.processes = after;
            let busy = if stable {
                actions::busy_count(&gauges, state.metrics_of(&self.module))
            } else {
                None
            };
            let budget_ms = state
                .roster
                .value
                .iter()
                .flatten()
                .find(|e| e.module_id == self.module)
                .and_then(|e| e.drain_timeout_ms)
                .unwrap_or(30_000);
            Ok(Reading {
                state,
                busy,
                busy_declared: !gauges.is_empty(),
                budget_ms,
            })
        })
    }

    fn send(
        &self,
        kind: ActionKind,
        module: &str,
        drain_timeout_ms: Option<u64>,
    ) -> Pin<Box<dyn Future<Output = Result<bool, CkError>> + Send + '_>> {
        let module = module.to_string();
        Box::pin(async move {
            let resolved = discover_connection_file(Some(&self.path))?;
            let mut client = CkClient::connect(resolved).await?;
            let request = match kind {
                ActionKind::Restart => ClientControlRequest::SupervisorRestart {
                    module_id: module.clone(),
                    drain_timeout_ms,
                },
                ActionKind::Stop | ActionKind::Start => {
                    ClientControlRequest::SupervisorSetEnabled {
                        module_id: module.clone(),
                        enabled: kind == ActionKind::Start,
                    }
                }
            };
            let timeout = if kind == ActionKind::Restart {
                CONTROL_RESPONSE_TIMEOUT
            } else {
                module_drain_response_timeout(&mut client, &module).await?
            };
            let ack = client
                .mutating_rpc_value(
                    request,
                    timeout,
                    format!("ck module {} {module}", kind.verb()),
                    format!("ck module status {module}"),
                )
                .await?;
            let applied = ack
                .get("applied")
                .and_then(Value::as_bool)
                .ok_or_else(|| CkError::Message(format!("unexpected action ack: {ack}")))?;
            if !self.inline {
                print_ack_with_state(&mut client, &module, ack, kind.verb(), false.into()).await?;
                if kind == ActionKind::Restart {
                    println!("restart initiated; verify: ck module status {module}");
                }
            }
            Ok(applied)
        })
    }
}

async fn spawns(
    client: &mut CkClient,
) -> Result<std::collections::HashMap<String, LiveProcess>, CkError> {
    let reply = client
        .rpc_value(ClientControlRequest::SupervisorSpawnSnapshot {})
        .await?;
    match serde_json::from_value(reply)? {
        ClientControlResponse::SupervisorSpawnSnapshot { snapshot } => Ok(snapshot
            .live
            .into_iter()
            .map(|p| {
                (
                    p.module_id,
                    LiveProcess {
                        pid: p.pid,
                        generation: p.spawn_generation,
                        spawned_at: Timestamp::from_unix_millis(p.spawned_at_ms as i64),
                    },
                )
            })
            .collect()),
        other => Err(CkError::Message(format!(
            "unexpected spawn snapshot: {other:?}"
        ))),
    }
}
