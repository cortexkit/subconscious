use super::*;
use crate::config_gate::runtime::{
    unstamped::USER_FILE_POLL_INTERVAL, RuntimeOwner, SettingsHistory,
};

const SETTINGS_QUIET_PERIOD: Duration = Duration::from_secs(30);
// The phone's ConfigPlanCard decoder refuses plans with more than 64 changes.
const PHONE_CARD_MAX_CHANGES: usize = 64;
const PHONE_CARD_TOO_LARGE: &str = "too many changes for the phone card";
const NO_APPROVING_DEVICE: &str = "no approving device to send the card to";

pub(super) struct AutomaticSettingsAttempt {
    pub(super) token: u64,
    pub(super) feedback: Option<tokio::sync::mpsc::UnboundedSender<CardFeedback>>,
}

#[derive(Clone, Copy)]
pub(super) enum CardFeedback {
    Filed,
}

fn card_size_refusal(fields: &[subc_config::FieldChange]) -> Option<&'static str> {
    (fields.len() > PHONE_CARD_MAX_CHANGES).then_some(PHONE_CARD_TOO_LARGE)
}

fn waiting_reason(history: &SettingsHistory) -> Option<&'static str> {
    if history.has_desktop_wait() {
        Some(PHONE_CARD_TOO_LARGE)
    } else {
        history.has_device_wait().then_some(NO_APPROVING_DEVICE)
    }
}

fn review_file_path(
    row: &subc_control::config::ConfigStatusFileError,
) -> Option<std::path::PathBuf> {
    row.error.path.as_ref().map(std::path::PathBuf::from)
}

impl ControlHandler {
    pub(crate) fn spawn_pending_settings_worker(
        self: &Arc<Self>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let runtime = self.config_runtime.clone()?;
        self.rescan.as_ref()?;
        let handler = self.clone();
        Some(tokio::spawn(async move {
            handler.run_pending_settings(runtime).await
        }))
    }

    async fn run_pending_settings(&self, runtime: Arc<RuntimeOwner>) {
        use crate::config_gate::runtime::{
            SettingsCardResult as Decision, SettingsEvent as Event, SettingsReviewResult as Review,
        };
        let mut poll = tokio::time::interval(USER_FILE_POLL_INTERVAL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut history = SettingsHistory::default();
        loop {
            poll.tick().await;
            let Ok(observed) = runtime.pending_settings_fingerprints().await else {
                continue;
            };
            let Some((base, files)) = observed else {
                history = SettingsHistory::default();
                continue;
            };
            let now = Instant::now();
            let changed = history.observe(&files, now, SETTINGS_QUIET_PERIOD);
            history.all(Event::BaseChanged(base));
            history.all(Event::QuietElapsed(now));
            let reason = waiting_reason(&history);
            let _ = config_work(runtime.clone(), ConfigWake::None, move |owner| {
                if !changed.is_empty() {
                    owner.clear_changed_settings_review_errors(&changed)?;
                }
                owner.set_pending_settings_card_reason(base, reason)
            })
            .await;

            let device = history.device_ready_paths(&files, now);
            let resuming = !device.is_empty() && self.config_phone_surface().await.is_some();
            let ready = if resuming {
                device
            } else {
                history.ready(&files)
            };
            if ready.is_empty() {
                continue;
            }
            history.each(
                ready.iter().cloned(),
                if resuming {
                    Event::DeviceAvailable { base }
                } else {
                    Event::ReviewStarted { base }
                },
            );
            let review_event = |current_base, result| Event::ReviewResult {
                base,
                current_base,
                result,
            };
            let capture = match runtime
                .prepare_settings_attempt(Some(files.witnesses(&ready)))
                .await
            {
                Ok(capture) => capture,
                Err(error) => {
                    tracing::debug!(code = %error.code, "settings file capture unavailable");
                    let current = runtime
                        .stamped_settings_base()
                        .unwrap_or(base.wrapping_add(1));
                    history.each(ready, review_event(current, Review::Retry(Instant::now())));
                    continue;
                }
            };
            let current = runtime
                .stamped_settings_base()
                .unwrap_or(base.wrapping_add(1));
            if current != base {
                history.each(ready, review_event(current, Review::Ready));
                continue;
            }
            history.each(capture.settled, review_event(base, Review::Settled));
            let Some(prepared) = capture.plan else {
                history.each(ready, Event::Capture { covered: false });
                continue;
            };
            if prepared.settings_review_base() != base {
                history.each(
                    ready,
                    review_event(prepared.settings_review_base(), Review::Ready),
                );
                continue;
            }
            let rows = prepared.settings_review_files();
            let attempted: std::collections::BTreeSet<_> =
                rows.iter().filter_map(review_file_path).collect();
            for path in &ready {
                history.apply(
                    path.clone(),
                    Event::Capture {
                        covered: attempted.contains(path),
                    },
                );
            }
            let reviewed = self.review_config_plan(runtime.clone(), prepared).await;
            let current = runtime
                .stamped_settings_base()
                .unwrap_or(base.wrapping_add(1));
            if current != base {
                history.each(attempted, review_event(current, Review::Ready));
                continue;
            }
            let view = match reviewed {
                Ok(view) if view.refusals.is_empty() => view,
                Ok(view) => {
                    let refused = rows
                        .into_iter()
                        .filter(|row| {
                            view.refusals.iter().any(|refusal| {
                                refusal.module_id == row.module_id
                                    && refusal.workspace_id.as_ref().is_none_or(|workspace| {
                                        Some(workspace) == row.workspace_id.as_ref()
                                    })
                            })
                        })
                        .collect();
                    self.finish_settings_refusals(&runtime, &mut history, base, attempted, refused)
                        .await;
                    continue;
                }
                Err(error) => {
                    tracing::debug!(code = %error.code, "settings file review unavailable");
                    if error.code == "settings_decline_kept" {
                        history.each(attempted, review_event(current, Review::Settled));
                    } else if matches!(
                        error.code.as_str(),
                        "module_review_refused" | "value_invalid" | "schema_invalid"
                    ) {
                        let refused = rows
                            .into_iter()
                            .filter(|row| {
                                review_file_path(row)
                                    .is_some_and(|path| error.settings_review_paths.contains(&path))
                            })
                            .collect();
                        self.finish_settings_refusals(
                            &runtime,
                            &mut history,
                            base,
                            attempted,
                            refused,
                        )
                        .await;
                    } else {
                        for path in attempted {
                            let result = if error.settings_review_paths.is_empty()
                                || error.settings_review_paths.contains(&path)
                            {
                                Review::Retry(Instant::now())
                            } else {
                                Review::Unfinished
                            };
                            history.apply(path, review_event(current, result));
                        }
                    }
                    continue;
                }
            };
            let covered: Vec<_> = view
                .settings_file_change
                .as_ref()
                .into_iter()
                .flat_map(|change| change.files.iter().map(|file| file.path.clone()))
                .collect();
            for path in &attempted {
                history.apply(
                    path.clone(),
                    Event::Capture {
                        covered: covered.contains(path),
                    },
                );
            }
            let digest = view.digest;
            let token = runtime.next_settings_request_order();
            let claimed = digest.clone();
            if config_work(runtime.clone(), ConfigWake::None, move |owner| {
                owner.claim_automatic_settings_plan(&claimed, token)
            })
            .await
            .is_err()
            {
                history.each(
                    covered,
                    review_event(current, Review::Retry(Instant::now())),
                );
                continue;
            }
            let unchanged = || async {
                runtime
                    .pending_settings_fingerprints()
                    .await
                    .is_ok_and(|current| {
                        current.is_some_and(|(revision, current)| {
                            revision == base && current.agrees_on(&files, &covered)
                        })
                    })
            };
            if !unchanged().await {
                history.each(covered, review_event(current, Review::Ready));
            } else if card_size_refusal(&view.plan.changeset.fields).is_some() {
                history.each(covered, review_event(current, Review::WaitForDesktop));
            } else if (!resuming && history.has_device_wait())
                || self.config_phone_surface().await.is_none()
            {
                history.each(
                    covered,
                    review_event(current, Review::WaitForDevice { order: token, now }),
                );
            } else {
                let reason = history.has_desktop_wait().then_some(PHONE_CARD_TOO_LARGE);
                let _ = config_work(runtime.clone(), ConfigWake::None, move |owner| {
                    owner.set_pending_settings_card_reason(base, reason)
                })
                .await;
                let (send, mut recv) = tokio::sync::mpsc::unbounded_channel();
                let apply = self.request_pending_settings_approval(
                    runtime.clone(),
                    digest.clone(),
                    AutomaticSettingsAttempt {
                        token,
                        feedback: Some(send),
                    },
                );
                tokio::pin!(apply);
                let mut filed = false;
                let mut record = |feedback| match feedback {
                    CardFeedback::Filed => {
                        filed = true;
                        history.each(
                            covered.iter().cloned(),
                            Event::CardFiled {
                                digest: digest.clone(),
                                base,
                            },
                        );
                    }
                };
                let result = loop {
                    tokio::select! {
                        biased;
                        Some(feedback) = recv.recv() => record(feedback),
                        result = &mut apply => break Some(result),
                        withdrawn = async {
                            poll.tick().await;
                            if !unchanged().await { return true; }
                            let digest = digest.clone();
                            config_work(runtime.clone(), ConfigWake::None, move |owner| owner.plan_get(&digest)).await.is_err()
                        } => if withdrawn { break None; },
                    }
                };
                while let Ok(feedback) = recv.try_recv() {
                    record(feedback);
                }
                let deciding = digest.clone();
                let kept = config_work(runtime.clone(), ConfigWake::None, move |owner| {
                    owner.settings_decline_accepted(&deciding, token)
                })
                .await
                .unwrap_or(true);
                let card_event = |result| Event::CardDecided {
                    digest: digest.clone(),
                    result,
                };
                if kept {
                    history.each(covered.iter().cloned(), card_event(Decision::Kept));
                }
                if !kept
                    || matches!(&result, Some(Err(error)) if error.code == "settings_card_declined")
                {
                    let now = Instant::now();
                    let result = match result {
                        Some(Ok(_)) => card_event(Decision::Settled),
                        Some(Err(error))
                            if matches!(
                                error.code.as_str(),
                                "settings_card_declined" | "settings_card_expired"
                            ) =>
                        {
                            card_event(Decision::Settled)
                        }
                        Some(Err(error))
                            if error.code == "approval_surface_unavailable"
                                && self.config_phone_surface().await.is_none() =>
                        {
                            if filed {
                                card_event(Decision::WaitForDevice { order: token, now })
                            } else {
                                review_event(current, Review::WaitForDevice { order: token, now })
                            }
                        }
                        Some(Err(error)) => {
                            tracing::debug!(code = %error.code, "settings file approval ended without applying");
                            if filed {
                                card_event(Decision::Retry(now))
                            } else {
                                review_event(current, Review::Retry(now))
                            }
                        }
                        None => {
                            if filed {
                                card_event(Decision::Withdrawn)
                            } else {
                                review_event(current, Review::Ready)
                            }
                        }
                    };
                    history.each(covered, result);
                }
            }
            let reason = waiting_reason(&history);
            let _ = config_work(runtime.clone(), ConfigWake::Delivery, move |owner| {
                owner.set_pending_settings_card_reason(base, reason)?;
                owner.withdraw_settings_card(&digest, token, false)
            })
            .await;
        }
    }

    async fn finish_settings_refusals(
        &self,
        runtime: &Arc<RuntimeOwner>,
        history: &mut SettingsHistory,
        base: u64,
        attempted: std::collections::BTreeSet<std::path::PathBuf>,
        refusals: Vec<subc_control::config::ConfigStatusFileError>,
    ) {
        use crate::config_gate::runtime::{SettingsEvent, SettingsReviewResult};
        let refused: std::collections::BTreeSet<_> =
            refusals.iter().filter_map(review_file_path).collect();
        let accepted = config_work(runtime.clone(), ConfigWake::None, move |owner| {
            owner.set_settings_review_errors(base, refusals)
        })
        .await
        .unwrap_or(false);
        let current_base = runtime
            .stamped_settings_base()
            .unwrap_or(base.wrapping_add(1));
        for path in attempted {
            let result = if accepted && refused.contains(&path) {
                SettingsReviewResult::Settled
            } else if refused.is_empty() && accepted {
                SettingsReviewResult::Retry(Instant::now())
            } else {
                SettingsReviewResult::Unfinished
            };
            history.apply(
                path,
                SettingsEvent::ReviewResult {
                    base,
                    current_base,
                    result,
                },
            );
        }
    }

    pub(super) async fn request_pending_settings_approval(
        &self,
        runtime: Arc<RuntimeOwner>,
        digest: subc_config::Digest,
        automatic: AutomaticSettingsAttempt,
    ) -> std::result::Result<ConfigControlResponse, GateError> {
        // Connection zero is not allocated to ingress. The placeholder frame
        // supplies queue bookkeeping only; no reply is sent to a client.
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);
        let ctx = RouteCtx {
            connection_id: ConnectionId::new(0),
            egress: crate::router::FrameSink::new(sender),
        };
        let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 1, Vec::new())
            .map_err(|error| GateError::new("config_unavailable", error))?;
        Box::pin(self.execute_config_request_from(
            &ctx,
            &frame,
            runtime,
            ConfigControlRequest::Apply { digest },
            Some(automatic),
        ))
        .await
    }

    pub(super) async fn revert_config_plan(
        &self,
        runtime: &Arc<RuntimeOwner>,
        digest: subc_config::Digest,
    ) -> std::result::Result<Vec<subc_control::config::ConfigRevertFile>, GateError> {
        let operation = self.supervisor.operation_lock().lock_owned().await;
        let runtime = runtime.clone();
        // Once files can be moved aside, finish the transaction and publish
        // any recovery paths even if the requesting client disconnects.
        tokio::spawn(async move {
            let _operation = operation;
            runtime.revert_capture_plan(digest).await
        })
        .await
        .map_err(|error| GateError::new("config_unavailable", error))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_pending_settings_phone_refuses_65_fields() {
        let fields = (1..=65)
            .map(|index| {
                subc_config::FieldChange::new(
                    "demo",
                    format!("/setting{index}"),
                    subc_config::RecordedValue::new(false.into()),
                    subc_config::RecordedValue::new(true.into()),
                    subc_config::ApplyKind::Live,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(card_size_refusal(&fields[..64]), None);
        assert_eq!(
            card_size_refusal(&fields),
            Some("too many changes for the phone card")
        );
    }
}
