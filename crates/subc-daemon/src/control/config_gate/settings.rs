use super::*;
use crate::config_gate::runtime::settings_observation::SettingsFiles;
use std::collections::{BTreeMap, BTreeSet};
use subc_control::config::{ConfigFileError, ConfigModule, ConfigWatchEvent, ConfigWatchResponse};

const SETTINGS_CAPTURE_SPACING: Duration = Duration::from_millis(250);
const SETTINGS_WATCH_LIMIT: usize = 4;
type SettingsRows = (Vec<ConfigModule>, bool, Arc<SettingsFiles>);

fn watch_refusal(
    streams: &HashMap<(ConnectionId, u64), tokio::task::AbortHandle>,
    key: (ConnectionId, u64),
    frame: &Frame,
) -> Result<Option<Frame>, RouterError> {
    let error = if streams.contains_key(&key) {
        control_error_frame(
            frame,
            "duplicate_subscription",
            "config.watch correlation id already in use",
        )?
    } else if streams.keys().filter(|(owner, _)| *owner == key.0).count() >= SETTINGS_WATCH_LIMIT {
        control_error_frame(
            frame,
            "too_many_watches",
            format!(
                "config.watch allows at most {SETTINGS_WATCH_LIMIT} subscriptions per connection"
            ),
        )?
    } else {
        return Ok(None);
    };
    Ok(Some(error))
}

#[derive(Default)]
struct SettingsFileGrace {
    pending: BTreeMap<(String, Option<String>), (ConfigFileError, tokio::time::Instant)>,
}

fn row_key(row: &ConfigModule) -> (&str, Option<&str>) {
    (&row.module_id, row.project_id.as_deref())
}

impl SettingsFileGrace {
    fn settle(
        &mut self,
        previous: &[ConfigModule],
        rows: &mut [ConfigModule],
        now: tokio::time::Instant,
    ) {
        self.pending.retain(|(module, project), _| {
            rows.iter().any(|row| {
                row_key(row) == (module.as_str(), project.as_deref()) && row.file_error.is_some()
            })
        });
        for row in rows {
            let Some(error) = row.file_error.clone() else {
                continue;
            };
            let candidate = self
                .pending
                .entry((row.module_id.clone(), row.project_id.clone()))
                .or_insert((error.clone(), now));
            if candidate.0 != error {
                *candidate = (error, now);
            }
            // Repeated publication wakes are not distinct save-settling ticks.
            // Confirm the same failure again at least one poll interval later.
            if now.duration_since(candidate.1)
                < crate::config_gate::runtime::unstamped::USER_FILE_POLL_INTERVAL
            {
                if let Some(old) = previous.iter().find(|old| row_key(old) == row_key(row)) {
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
        let mut known = self
            .registry
            .registered_module_ids()
            .map_err(|_| GateError::new("config_unavailable", "module registry is unavailable"))?;
        // A prefix registration alone does not imply local settings. The settings
        // runtime adds declared and approved settings independently of registration.
        self.supervisor.remove_reserved_prefix_children(&mut known);
        Ok(known)
    }

    pub(super) async fn handle_config_settings_watch(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        runtime: Arc<crate::config_gate::runtime::RuntimeOwner>,
        modules: Option<Vec<String>>,
        projects: bool,
    ) -> Result<Vec<Frame>, RouterError> {
        let key = (ctx.connection_id, frame.header.corr);
        {
            let mut streams = self
                .config_settings_streams
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            streams.retain(|_, task| !task.is_finished());
            if let Some(error) = watch_refusal(&streams, key, &frame)? {
                return Ok(vec![error]);
            }
        }
        let mut changes = runtime.subscribe_settings();
        let mut catalog = self
            .registry
            .subscribe_catalog_changes()
            .map_err(|error| RouterError::backend(0, frame.header.corr, error.to_string()))?;
        let mut settings_version = *changes.borrow_and_update();
        let mut catalog_version = *catalog.borrow_and_update();
        if projects {
            let _ = runtime.refresh_settings_projects(true).await;
        }
        let mut initial = match self
            .config_settings_initial_rows(runtime.clone(), &modules, projects)
            .await
        {
            Ok(initial) => initial,
            Err(error) => return Ok(vec![config_error_frame(&frame, error)?]),
        };
        let mut file_grace = SettingsFileGrace::default();
        let mut latest = initial.0.clone();
        let settling = initial.0.iter().any(|row| row.file_error.is_some());
        file_grace.settle(&[], &mut initial.0, tokio::time::Instant::now());
        if settling {
            tokio::time::sleep(crate::config_gate::runtime::unstamped::USER_FILE_POLL_INTERVAL)
                .await;
            if projects {
                let _ = runtime.refresh_settings_projects(true).await;
            }
            initial = match self
                .config_settings_initial_rows(runtime.clone(), &modules, projects)
                .await
            {
                Ok(rows) => rows,
                Err(error) => return Ok(vec![config_error_frame(&frame, error)?]),
            };
            latest = initial.0.clone();
            file_grace.settle(&[], &mut initial.0, tokio::time::Instant::now());
        }
        let mut streams = self
            .config_settings_streams
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        streams.retain(|_, task| !task.is_finished());
        if let Some(error) = watch_refusal(&streams, key, &frame)? {
            return Ok(vec![error]);
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
            let (mut previous, mut stamped, mut files) = initial;
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
            let mut next_capture = None;
            let project_interval = crate::config_gate::runtime::unstamped::USER_FILE_POLL_INTERVAL;
            let mut next_project_capture = tokio::time::Instant::now() + project_interval;
            loop {
                tokio::select! {
                    result = changes.changed(), if !retry_capture => if result.is_err() { return; },
                    result = catalog.changed(), if !retry_capture => if result.is_err() { return; },
                    _ = poll.tick() => {},
                    _ = tokio::time::sleep_until(next_project_capture), if projects => {},
                }
                if let Some(at) = next_capture {
                    tokio::time::sleep_until(at).await;
                }
                if sink.is_closed() {
                    return;
                }
                if projects && tokio::time::Instant::now() >= next_project_capture {
                    let _ = runtime.refresh_settings_projects(true).await;
                    next_project_capture = tokio::time::Instant::now() + project_interval;
                }
                // Coalesce both change sources before reading. Changes during
                // the capture remain unread for the next pass.
                let new_settings_version = *changes.borrow_and_update();
                let new_catalog_version = *catalog.borrow_and_update();
                let changed = retry_capture
                    || new_settings_version != settings_version
                    || new_catalog_version != catalog_version
                    || runtime.settings_projection_stale();
                settings_version = new_settings_version;
                catalog_version = new_catalog_version;
                next_capture = Some(tokio::time::Instant::now() + SETTINGS_CAPTURE_SPACING);
                let next = if changed {
                    handler
                        .config_settings_rows(runtime.clone(), &modules, projects)
                        .await
                } else {
                    let probe = Arc::clone(&files);
                    match runtime
                        .on_blocking(move |_| Ok(probe.needs_observation()))
                        .await
                    {
                        Ok(false) => Ok(Some((latest.clone(), stamped, Arc::clone(&files)))),
                        Ok(true) => {
                            handler
                                .config_settings_rows(runtime.clone(), &modules, projects)
                                .await
                        }
                        Err(error) => Err(error),
                    }
                };
                let (mut rows, next_stamped, next_files) = match next {
                    Ok(Some(next)) => next,
                    Ok(None) => {
                        // No consistent observation was made. Keep the stream
                        // open without advancing the file-error grace period.
                        retry_capture = true;
                        file_grace.pending.clear();
                        poll.reset();
                        continue;
                    }
                    Err(error) if error.is_capture_race() => {
                        // A read interrupted by a file edit is not a new state.
                        // Keep the last snapshot and retry on the next poll tick.
                        retry_capture = true;
                        file_grace.pending.clear();
                        poll.reset();
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
                latest = rows.clone();
                files = next_files;
                file_grace.settle(&previous, &mut rows, tokio::time::Instant::now());
                // The next confirmation is timed from the completed capture,
                // not from when the (possibly slower) full observation started.
                poll.reset();
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
        projects: bool,
    ) -> std::result::Result<SettingsRows, GateError> {
        let error = match self
            .config_settings_rows(runtime.clone(), filter, projects)
            .await
        {
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
                match self
                    .config_settings_rows(runtime.clone(), filter, projects)
                    .await
                {
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
        projects: bool,
    ) -> std::result::Result<Option<SettingsRows>, GateError> {
        let known = self.config_settings_known_modules()?;
        let response = runtime
            .on_blocking(move |owner| {
                let response = owner.observe_settings_rows(known, true, projects);
                #[cfg(test)]
                if let Some(hook) = owner.settings_observation_hook.lock().unwrap().as_mut() {
                    hook();
                }
                response
            })
            .await?;
        let Some(observation) = response else {
            return Ok(None);
        };
        let ConfigControlResponse::Modules {
            mut modules,
            stamped,
        } = observation.response
        else {
            unreachable!()
        };
        if let Some(filter) = filter {
            modules.retain(|module| filter.contains(&module.module_id));
        }
        Ok(Some((modules, stamped, Arc::new(observation.files))))
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
    let previous_by_key: BTreeMap<_, _> = previous.iter().map(|row| (row_key(row), row)).collect();
    if stamped != next_stamped
        || previous.len() != rows.len()
        || rows
            .iter()
            .any(|row| !previous_by_key.contains_key(&row_key(row)))
    {
        return vec![ConfigWatchEvent::Resync {
            modules: rows.to_vec(),
            stamped: next_stamped,
        }];
    }
    rows.iter()
        .filter(|row| {
            previous_by_key
                .get(&row_key(row))
                .is_none_or(|old| ConfigWatchEvent::change(old) != ConfigWatchEvent::change(row))
        })
        .map(ConfigWatchEvent::change)
        .collect()
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;
