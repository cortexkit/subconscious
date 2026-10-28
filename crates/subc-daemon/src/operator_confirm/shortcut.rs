//! Audit-gated shortcuts keep the ordinary confirmation request id and use the
//! same withdrawal handling.
use super::*;
use crate::{
    activity::ActivityLedger,
    app_open::LaunchTable,
    forwarding::ForwardingTable,
    operator_apps::{OperatorAppPolicy, OperatorAppStamp},
};
use subc_control::{ActivityActor, ActivityRecordRequest};
use tokio::sync::watch;

#[derive(Clone)]
pub(crate) struct ShortcutContext {
    pub ledger: Option<ActivityLedger>,
    pub forwarding: Arc<ForwardingTable>,
    pub launches: Arc<LaunchTable>,
    pub stamp: Option<OperatorAppStamp>,
    pub phone_fingerprint: Option<String>,
    pub route: Option<(u16, u32)>,
    pub connections: Arc<Mutex<HashMap<ConnectionId, OperatorAppStamp>>>,
    #[cfg(test)]
    pub hooks: Hooks,
}

impl ShortcutContext {
    pub(crate) fn route_eligible(
        policy: &OperatorAppPolicy,
        launches: &LaunchTable,
        binding: &RouteBinding,
    ) -> bool {
        Self::eligible_stamp(policy, launches, binding.operator_app.as_ref())
            || Self::phone_fingerprint(policy, binding).is_some()
    }

    pub(crate) fn eligible_stamp(
        policy: &OperatorAppPolicy,
        launches: &LaunchTable,
        stamp: Option<&OperatorAppStamp>,
    ) -> bool {
        stamp.is_some_and(|stamp| {
            stamp.delivery_enabled(policy) && stamp.revalidate_with_launches(launches)
        })
    }

    pub(crate) fn phone_fingerprint(
        policy: &OperatorAppPolicy,
        binding: &RouteBinding,
    ) -> Option<String> {
        if !policy.phone_enabled
            || !matches!(&binding.principal, Principal::Reserved { module_id } if module_id == "callosum")
            || binding.build_class.as_deref() != Some("production")
        {
            return None;
        }
        let fingerprint = binding.harness.strip_prefix("fed-phone:")?;
        (fingerprint.len() == 32
            && fingerprint
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        .then(|| fingerprint.to_owned())
    }

    fn revalidate(&self) -> bool {
        self.stamp
            .as_ref()
            .is_none_or(|stamp| stamp.revalidate_with_launches(&self.launches))
    }

    fn with_incarnation<T>(&self, commit: impl FnOnce() -> T) -> Option<T> {
        match self
            .stamp
            .as_ref()
            .and_then(|stamp| stamp.launch_incarnation)
        {
            Some(incarnation) => self.launches.with_live_incarnation(
                &self.stamp.as_ref().unwrap().app.app,
                incarnation,
                commit,
            ),
            None => Some(commit()),
        }
    }

    async fn record(
        &self,
        attempt: &str,
        operation: &str,
        outcome: Option<&str>,
    ) -> Result<(), &'static str> {
        let Some(ledger) = &self.ledger else {
            return Err("activity_unavailable");
        };
        let (kind, summary, details) = if let Some(outcome) = outcome {
            (
                "operator_shortcut_outcome",
                "Operator shortcut ended",
                serde_json::json!({"attempt": attempt, "outcome": outcome}),
            )
        } else {
            let mut details = serde_json::json!({"attempt": attempt, "operation": operation});
            if let Some(fingerprint) = &self.phone_fingerprint {
                details["via"] = "phone".into();
                details["phone_fingerprint"] = fingerprint.clone().into();
            } else {
                details["via"] = "operator_app".into();
                details["app"] = self.stamp.as_ref().unwrap().app.app.clone().into();
            }
            ("operator_shortcut", "Operator shortcut attempted", details)
        };
        ledger
            .record(
                "subc".into(),
                ActivityRecordRequest {
                    kind: kind.into(),
                    summary: summary.into(),
                    subject: None,
                    details: details.as_object().cloned(),
                    on: None,
                },
                ActivityActor {
                    principal: Principal::Direct,
                    scope_owner: None,
                    agent_id: None,
                    run_id: None,
                    flow_id: None,
                    project_root: None,
                    workdir: None,
                    project_id: None,
                },
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
            )
            .await
            .map(|_| ())
    }
}

pub(super) struct Pending {
    pub(super) request: Arc<Request>,
    ended: watch::Sender<Option<&'static str>>,
}

enum Decision {
    Ended(&'static str),
    Handoff,
    Queued,
}

impl OperatorConfirms {
    pub(super) fn start_shortcut(
        self: &Arc<Self>,
        state: &mut State,
        request: Arc<Request>,
        context: ShortcutContext,
    ) {
        let (ended, mut ending) = watch::channel(None);
        state.shortcuts.insert(
            request.id,
            Pending {
                request: request.clone(),
                ended,
            },
        );
        // A separate timer keeps answering while an append or a test pause is held.
        let this = self.clone();
        let timer_request = request.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep_until(timer_request.delivery_deadline) => {
                    let mut state = this.lock();
                    this.end_shortcut(&mut state, timer_request.id, Outcome::unavailable("timeout"), "timeout");
                }
                _ = ending.changed() => {}
            }
        });
        let this = self.clone();
        let ending = state.shortcuts[&request.id].ended.subscribe();
        tokio::spawn(async move {
            this.run_shortcut(request, context, ending).await;
        });
    }

    pub(super) fn end_shortcut(
        self: &Arc<Self>,
        state: &mut State,
        id: u64,
        answer: Outcome,
        outcome: &'static str,
    ) {
        if let Some(pending) = state.shortcuts.remove(&id) {
            pending.ended.send_replace(Some(outcome));
            self.commit(state, pending.request, answer, None, false);
        }
    }

    // The forwarding/connection lock is already held by the caller. No await or
    // provider call can slip between validation and removing the pending request.
    fn decide_shortcut(
        self: &Arc<Self>,
        request: &Arc<Request>,
        context: &ShortcutContext,
        route_error: Option<&'static str>,
        handoff: bool,
    ) -> Decision {
        let mut state = self.lock();
        if !state.shortcuts.contains_key(&request.id) {
            return Decision::Ended("withdrawn"); // The recorded ending is read by the task.
        }
        if Instant::now() >= request.delivery_deadline {
            self.end_shortcut(
                &mut state,
                request.id,
                Outcome::unavailable("timeout"),
                "timeout",
            );
            return Decision::Ended("timeout");
        }
        if let Some(outcome) = route_error {
            self.end_shortcut(
                &mut state,
                request.id,
                Outcome::refusal(error_codes::OPERATOR_REQUEST_NOT_PERMITTED),
                outcome,
            );
            return Decision::Ended(outcome);
        }
        if !handoff {
            // Removing the pending request is the authorization commit point.
            // Deliver only after releasing the launch-table lock.
            let Some((pending, expired)) = context.with_incarnation(|| {
                (
                    state.shortcuts.remove(&request.id),
                    Instant::now() >= request.delivery_deadline,
                )
            }) else {
                return Decision::Handoff;
            };
            let pending = pending.expect("pending checked under the state lock");
            let outcome = if expired { "timeout" } else { "confirmed" };
            pending.ended.send_replace(Some(outcome));
            self.commit(
                &mut state,
                pending.request,
                if expired {
                    Outcome::unavailable("timeout")
                } else {
                    Outcome::from_provider(ProviderResult::Approved)
                },
                None,
                false,
            );
            return Decision::Ended(outcome);
        }
        let now = Instant::now();
        let refusal = if state.stuck {
            Some(Outcome::unavailable("provider_stuck"))
        } else if state.queue.len() >= 4 {
            Some(Outcome::unavailable("queue_full"))
        } else if let RequestKind::Privileged { key, .. } = &request.kind {
            state
                .cooldowns
                .get(key)
                .filter(|until| **until > (state.cooldown_clock)())
                .map(|_| Outcome::declined("backoff"))
        } else {
            state
                .backoff
                .get(&(request.module_id.clone(), request.principal.clone()))
                .filter(|until| **until > now)
                .map(|until| {
                    Outcome::declined("backoff").with_retry_after_ms(retry_after_ms(*until - now))
                })
        };
        #[cfg(test)]
        let refusal = if context.hooks.handoff_refused {
            Some(Outcome::unavailable("queue_full"))
        } else {
            refusal
        };
        if let Some(refusal) = refusal {
            self.end_shortcut(&mut state, request.id, refusal, "handoff_refused");
            return Decision::Ended("handoff_refused");
        }
        let pending = state
            .shortcuts
            .remove(&request.id)
            .expect("pending checked");
        pending.ended.send_replace(Some("prompted"));
        request
            .prompt_ready
            .store(false, std::sync::atomic::Ordering::Release);
        self.enqueue(&mut state, request.clone());
        Decision::Queued
    }

    fn check_shortcut(
        self: &Arc<Self>,
        request: &Arc<Request>,
        context: &ShortcutContext,
        handoff: bool,
    ) -> Decision {
        if let Some((channel, epoch)) = context.route {
            context
                .forwarding
                .with_operator_route_epoch(request.ctx.connection_id, channel, |binding| {
                    let error = match binding {
                        None => Some("closed"),
                        Some(binding) if binding.module_epoch != epoch => Some("stale"),
                        Some(_) => None,
                    };
                    self.decide_shortcut(request, context, error, handoff)
                })
                .unwrap_or_else(|_| self.decide_shortcut(request, context, Some("closed"), handoff))
        } else {
            // Connection removal takes this lock before notifying withdrawals.
            let connections = context
                .connections
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if !connections.contains_key(&request.ctx.connection_id)
                || request.ctx.egress.is_closed()
            {
                let mut state = self.lock();
                self.end_shortcut(
                    &mut state,
                    request.id,
                    Outcome::declined("caller_cancelled"),
                    "withdrawn",
                );
                Decision::Ended("withdrawn")
            } else {
                self.decide_shortcut(request, context, None, handoff)
            }
        }
    }
}

impl OperatorConfirms {
    async fn run_shortcut(
        self: Arc<Self>,
        request: Arc<Request>,
        context: ShortcutContext,
        ending: watch::Receiver<Option<&'static str>>,
    ) {
        let operation = match request.kind {
            RequestKind::Module(_) => format!("operator.confirm:{}", request.module_id),
            RequestKind::Privileged { .. } => request.module_id.clone(),
        };
        let mut random = [0u8; 16];
        let attempt = if getrandom::getrandom(&mut random).is_ok() {
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        } else {
            // Without a unique audit identity there can be no shortcut grant.
            String::new()
        };
        #[cfg(test)]
        context.hooks.wait_before_append().await;
        let append = if attempt.is_empty() {
            Err("random_unavailable")
        } else {
            #[cfg(test)]
            if context.hooks.reject_attempt {
                Err("test_prewrite_rejection")
            } else {
                context.record(&attempt, &operation, None).await
            }
            #[cfg(not(test))]
            context.record(&attempt, &operation, None).await
        };
        #[cfg(test)]
        let append = if append.is_ok() && context.hooks.substitute_error {
            Err("test_unknown_append")
        } else {
            append
        };
        #[cfg(test)]
        context.hooks.wait_after_append().await;

        // Withdrawal and timeout win even if the append subsequently fails.
        let mut fixed = {
            let mut state = self.lock();
            if Instant::now() >= request.delivery_deadline {
                self.end_shortcut(
                    &mut state,
                    request.id,
                    Outcome::unavailable("timeout"),
                    "timeout",
                );
            }
            *ending.borrow()
        };
        let mut decision = if let Some(outcome) = fixed {
            Decision::Ended(outcome)
        } else if append.is_err() {
            fixed = Some("append_failed");
            #[cfg(test)]
            context.hooks.wait_before_handoff().await;
            self.check_shortcut(&request, &context, true)
        } else if !context.revalidate() {
            #[cfg(test)]
            context.hooks.wait_before_handoff().await;
            self.check_shortcut(&request, &context, true)
        } else {
            #[cfg(test)]
            context.hooks.wait_after_revalidate().await;
            self.check_shortcut(&request, &context, false)
        };
        if matches!(decision, Decision::Handoff) {
            #[cfg(test)]
            context.hooks.wait_before_handoff().await;
            decision = self.check_shortcut(&request, &context, true);
        }
        let outcome = fixed.unwrap_or_else(|| {
            (*ending.borrow()).unwrap_or(match decision {
                Decision::Ended(outcome) => outcome,
                Decision::Queued => "prompted",
                Decision::Handoff => unreachable!(),
            })
        });
        if let Err(error) = append {
            tracing::warn!(
                error,
                "operator shortcut attempt append failed; shortcut cannot grant"
            );
        }
        if context.ledger.is_some() && !attempt.is_empty() {
            #[cfg(test)]
            let write_outcome = !context.hooks.fail_outcome;
            #[cfg(not(test))]
            let write_outcome = true;
            let result = if write_outcome {
                context.record(&attempt, &operation, Some(outcome)).await
            } else {
                Err("test_outcome_failure")
            };
            if let Err(error) = result {
                tracing::warn!(error, outcome, "operator shortcut outcome append failed");
            }
        }
        // Enqueue was atomic with the handoff re-check, but the provider must
        // not run before the best-effort outcome append has been attempted.
        if matches!(decision, Decision::Queued) {
            request
                .prompt_ready
                .store(true, std::sync::atomic::Ordering::Release);
            self.wake.notify_one();
        }
        #[cfg(test)]
        if let Some(settled) = &context.hooks.settled {
            settled.notify_one();
        }
    }
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct Hooks {
    pub before_append: Option<Arc<Gate>>,
    pub after_append: Option<Arc<Gate>>,
    pub after_revalidate: Option<Arc<Gate>>,
    pub before_handoff: Option<Arc<Gate>>,
    pub reject_attempt: bool,
    pub substitute_error: bool,
    pub fail_outcome: bool,
    pub handoff_refused: bool,
    pub settled: Option<Arc<Notify>>,
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct Gate {
    pub entered: Notify,
    pub release: Notify,
}

#[cfg(test)]
impl Hooks {
    async fn wait(gate: &Option<Arc<Gate>>) {
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
    }
    async fn wait_before_append(&self) {
        Self::wait(&self.before_append).await;
    }
    async fn wait_after_append(&self) {
        Self::wait(&self.after_append).await;
    }
    async fn wait_after_revalidate(&self) {
        Self::wait(&self.after_revalidate).await;
    }
    async fn wait_before_handoff(&self) {
        Self::wait(&self.before_handoff).await;
    }
}
