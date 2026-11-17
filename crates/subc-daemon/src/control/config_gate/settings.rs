use super::*;
use std::collections::{BTreeMap, BTreeSet};
use subc_control::config::{ConfigFileError, ConfigModule, ConfigWatchEvent, ConfigWatchResponse};

#[derive(Default)]
struct SettingsFileGrace {
    pending: BTreeMap<String, (ConfigFileError, tokio::time::Instant)>,
}

impl SettingsFileGrace {
    fn settle(
        &mut self,
        previous: &[ConfigModule],
        rows: &mut [ConfigModule],
        now: tokio::time::Instant,
    ) {
        self.pending.retain(|module, _| {
            rows.iter()
                .any(|row| &row.module_id == module && row.file_error.is_some())
        });
        for row in rows {
            let Some(error) = row.file_error.clone() else {
                continue;
            };
            let candidate = self
                .pending
                .entry(row.module_id.clone())
                .or_insert((error.clone(), now));
            if candidate.0 != error {
                *candidate = (error, now);
            }
            // Repeated publication wakes are not distinct save-settling ticks.
            // Confirm the same failure again at least one poll interval later.
            if now.duration_since(candidate.1)
                < crate::config_gate::runtime::unstamped::USER_FILE_POLL_INTERVAL
            {
                if let Some(old) = previous.iter().find(|old| old.module_id == row.module_id) {
                    *row = old.clone();
                } else {
                    row.file_error = None;
                }
            }
        }
    }
}

impl ControlHandler {
    pub(super) fn config_settings_known_modules(
        &self,
    ) -> std::result::Result<BTreeSet<String>, GateError> {
        self.registry
            .list_registered_modules()
            .map(|modules| {
                modules
                    .into_iter()
                    .map(|module| module.manifest.module_id)
                    .collect()
            })
            .map_err(|_| GateError::new("config_unavailable", "module registry is unavailable"))
    }

    pub(super) async fn handle_config_settings_watch(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        runtime: Arc<crate::config_gate::runtime::RuntimeOwner>,
        modules: Option<Vec<String>>,
    ) -> Result<Vec<Frame>, RouterError> {
        let mut changes = runtime.subscribe_settings();
        let mut catalog = self
            .registry
            .subscribe_catalog_changes()
            .map_err(|error| RouterError::backend(0, frame.header.corr, error.to_string()))?;
        let mut initial = match self
            .config_settings_initial_rows(runtime.clone(), &modules)
            .await
        {
            Ok(initial) => initial,
            Err(error) => return Ok(vec![config_error_frame(&frame, error)?]),
        };
        let mut file_grace = SettingsFileGrace::default();
        let settling = initial.0.iter().any(|row| row.file_error.is_some());
        file_grace.settle(&[], &mut initial.0, tokio::time::Instant::now());
        if settling {
            tokio::time::sleep(crate::config_gate::runtime::unstamped::USER_FILE_POLL_INTERVAL)
                .await;
            initial = match self
                .config_settings_initial_rows(runtime.clone(), &modules)
                .await
            {
                Ok(rows) => rows,
                Err(error) => return Ok(vec![config_error_frame(&frame, error)?]),
            };
            file_grace.settle(&[], &mut initial.0, tokio::time::Instant::now());
        }
        let key = (ctx.connection_id, frame.header.corr);
        let mut streams = self
            .config_settings_streams
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        streams.retain(|_, task| !task.is_finished());
        if streams.contains_key(&key) {
            return Ok(vec![control_error_frame(
                &frame,
                "duplicate_subscription",
                "config.watch correlation id already in use",
            )?]);
        }
        let handler = self.clone();
        let sink = ctx.egress.clone();
        let task = tokio::spawn(async move {
            let push = |event| -> Result<Frame, RouterError> {
                let body =
                    serde_json::to_vec(&ConfigWatchResponse::new(event)).map_err(|error| {
                        RouterError::backend(0, frame.header.corr, error.to_string())
                    })?;
                Frame::build_with_version(
                    response_version(&frame),
                    FrameType::StreamData,
                    control_flags(),
                    0,
                    0,
                    frame.header.corr,
                    body,
                )
                .map_err(|error| RouterError::backend(0, frame.header.corr, error.to_string()))
            };
            let (mut previous, mut stamped) = initial;
            let Ok(first) = push(ConfigWatchEvent::Resync {
                modules: previous.clone(),
                stamped,
            }) else {
                return;
            };
            if sink.send(first).await.is_err() {
                return;
            }
            // File proposals can change without an owner publication. Match the
            // existing user-file observer's cadence while also waking on commits.
            let mut poll = tokio::time::interval(
                crate::config_gate::runtime::unstamped::USER_FILE_POLL_INTERVAL,
            );
            poll.tick().await;
            let mut retry_capture = false;
            loop {
                tokio::select! {
                    result = changes.changed(), if !retry_capture => if result.is_err() { return; },
                    result = catalog.changed(), if !retry_capture => if result.is_err() { return; },
                    _ = poll.tick() => {},
                }
                if sink.is_closed() {
                    return;
                }
                let next = handler
                    .config_settings_rows(runtime.clone(), &modules)
                    .await;
                let (mut rows, next_stamped) = match next {
                    Ok(Some(next)) => next,
                    Ok(None) => {
                        // No consistent observation was made. Keep the stream
                        // open without advancing the file-error grace period.
                        retry_capture = true;
                        file_grace.pending.clear();
                        continue;
                    }
                    Err(error) if error.is_capture_race() => {
                        // A read interrupted by a file edit is not a new state.
                        // Keep the last snapshot and retry on the next poll tick.
                        retry_capture = true;
                        file_grace.pending.clear();
                        continue;
                    }
                    Err(error) => {
                        if let Ok(error) = config_error_frame(&frame, error) {
                            let _ = sink.send(error).await;
                        }
                        return;
                    }
                };
                retry_capture = false;
                file_grace.settle(&previous, &mut rows, tokio::time::Instant::now());
                for event in settings_events(&previous, stamped, &rows, next_stamped) {
                    let Ok(next) = push(event) else {
                        return;
                    };
                    if sink.send(next).await.is_err() {
                        return;
                    }
                }
                previous = rows;
                stamped = next_stamped;
            }
        });
        streams.insert(key, task.abort_handle());
        Ok(Vec::new())
    }

    async fn config_settings_initial_rows(
        &self,
        runtime: Arc<crate::config_gate::runtime::RuntimeOwner>,
        filter: &Option<Vec<String>>,
    ) -> std::result::Result<(Vec<ConfigModule>, bool), GateError> {
        let error = match self.config_settings_rows(runtime.clone(), filter).await {
            Ok(Some(rows)) => return Ok(rows),
            Ok(None) => GateError::capture_race(),
            Err(error) if error.is_capture_race() => error,
            Err(error) => return Err(error),
        };
        // Give a competing writer a few short retries, but do not leave an
        // opening subscriber waiting indefinitely without its first resync.
        let fallback = error.clone();
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut last = error;
            for delay in [50, 100, 200] {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                match self.config_settings_rows(runtime.clone(), filter).await {
                    Ok(Some(rows)) => return Ok(rows),
                    Ok(None) => last = GateError::capture_race(),
                    Err(error) if error.is_capture_race() => last = error,
                    Err(error) => return Err(error),
                }
            }
            Err(last)
        })
        .await
        .unwrap_or(Err(fallback))
    }

    async fn config_settings_rows(
        &self,
        runtime: Arc<crate::config_gate::runtime::RuntimeOwner>,
        filter: &Option<Vec<String>>,
    ) -> std::result::Result<Option<(Vec<ConfigModule>, bool)>, GateError> {
        let known = self.config_settings_known_modules()?;
        let response = config_work(runtime, move |owner| {
            let response = owner.observe_settings_modules(known);
            #[cfg(test)]
            if let Some(hook) = owner.settings_observation_hook.lock().unwrap().as_mut() {
                hook();
            }
            response
        })
        .await?;
        let Some(response) = response else {
            return Ok(None);
        };
        let ConfigControlResponse::Modules {
            mut modules,
            stamped,
        } = response
        else {
            unreachable!()
        };
        if let Some(filter) = filter {
            modules.retain(|module| filter.contains(&module.module_id));
        }
        Ok(Some((modules, stamped)))
    }

    pub(in crate::control) fn cancel_config_settings_subscription(
        &self,
        connection: ConnectionId,
        corr: u64,
    ) -> bool {
        if let Some(task) = self
            .config_settings_streams
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&(connection, corr))
        {
            task.abort();
            true
        } else {
            false
        }
    }

    pub(in crate::control) fn close_config_settings_subscriptions(&self, connection: ConnectionId) {
        self.config_settings_streams
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(owner, _), task| {
                if *owner == connection {
                    task.abort();
                    false
                } else {
                    true
                }
            });
    }
}

fn settings_events(
    previous: &[ConfigModule],
    stamped: bool,
    rows: &[ConfigModule],
    next_stamped: bool,
) -> Vec<ConfigWatchEvent> {
    if stamped != next_stamped
        || previous.len() != rows.len()
        || previous
            .iter()
            .any(|old| !rows.iter().any(|row| row.module_id == old.module_id))
    {
        return vec![ConfigWatchEvent::Resync {
            modules: rows.to_vec(),
            stamped: next_stamped,
        }];
    }
    rows.iter()
        .filter(|row| {
            previous
                .iter()
                .find(|old| old.module_id == row.module_id)
                .is_none_or(|old| ConfigWatchEvent::change(old) != ConfigWatchEvent::change(row))
        })
        .map(ConfigWatchEvent::change)
        .collect()
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;
