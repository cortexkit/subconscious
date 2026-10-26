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
    force_wait: bool,
) -> Result<(), CkError> {
    let not_following = if inline && !force_wait {
        match spawns(client).await {
            Ok(processes) => module_ancestor(std::process::id(), &processes, subc_os::parent_pid)
                .map(|module| format!("Not following: this shell runs inside {module}, so waiting could hold a drain. Check it with ck module list.")),
            Err(_) => Some("Not following: ck couldn't read the module roster.".into()),
        }
    } else {
        None
    };
    let source = CliAction {
        path: client.path.clone(),
        client: tokio::sync::Mutex::new(client),
        module: module.into(),
        gauges: std::sync::Mutex::new(None),
        inline,
    };
    if let Some(reason) = not_following {
        source.send(kind, module, drain_timeout_ms).await?;
        println!("{reason}");
        return Ok(());
    }
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

fn module_ancestor(
    mut pid: u32,
    processes: &std::collections::HashMap<String, LiveProcess>,
    mut parent: impl FnMut(u32) -> Option<u32>,
) -> Option<String> {
    let mut seen = std::collections::HashSet::new();
    seen.insert(pid);
    for _ in 0..64 {
        if pid <= 1 {
            return None;
        }
        pid = parent(pid)?;
        if pid <= 1 || !seen.insert(pid) {
            return None;
        }
        // A tool shell may belong to a different module than the action target.
        if let Some((module, _)) = processes.iter().find(|(_, process)| process.pid == pid) {
            return Some(module.clone());
        }
    }
    None
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

#[cfg(test)]
mod ancestry_tests {
    use super::*;

    fn process_at(pid: u32) -> std::collections::HashMap<String, LiveProcess> {
        std::collections::HashMap::from([(
            "tool".into(),
            LiveProcess {
                pid,
                generation: 1,
                spawned_at: Timestamp::from_unix_millis(1000),
            },
        )])
    }

    #[test]
    fn a_root_or_current_pid_is_not_a_module_ancestor() {
        for pid in [0, 1, 10] {
            assert_eq!(module_ancestor(10, &process_at(pid), |_| Some(pid)), None);
        }
    }

    #[test]
    fn the_64th_ancestor_matches_but_the_65th_does_not() {
        assert_eq!(
            module_ancestor(10, &process_at(74), |pid| Some(pid + 1)),
            Some("tool".into())
        );
        assert_eq!(
            module_ancestor(10, &process_at(75), |pid| Some(pid + 1)),
            None
        );
    }

    #[test]
    fn a_cycle_is_not_inside_a_module() {
        let mut steps = 0;
        assert_eq!(
            module_ancestor(10, &Default::default(), |pid| {
                steps += 1;
                Some(if pid == 10 { 20 } else { 10 })
            }),
            None
        );
        assert_eq!(steps, 2);
    }

    #[test]
    fn ancestry_stops_at_roots_missing_parents_and_64_steps() {
        for root in [0, 1] {
            let mut steps = 0;
            assert_eq!(
                module_ancestor(10, &Default::default(), |_| {
                    steps += 1;
                    Some(root)
                }),
                None
            );
            assert_eq!(steps, 1);
            assert_eq!(
                module_ancestor(root, &Default::default(), |_| panic!("root has no parent")),
                None
            );
        }
        assert_eq!(module_ancestor(10, &Default::default(), |_| None), None);
        let mut steps = 0;
        assert_eq!(
            module_ancestor(10, &Default::default(), |pid| {
                steps += 1;
                Some(pid + 1)
            }),
            None
        );
        assert_eq!(steps, 64);
    }
}
