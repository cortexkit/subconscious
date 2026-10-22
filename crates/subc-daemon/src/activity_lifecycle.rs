use super::*;
use subc_control::{ActivitySubject, RunningImageAgreement, RunningImageEvidence};
use subc_protocol::Principal;

#[derive(Default)]
pub(super) struct LifecycleCounters {
    failed: AtomicU64,
    digest_skipped: AtomicU64,
}

pub(super) struct PendingLifecycle {
    counters: Arc<LifecycleCounters>,
    kind: String,
    evidence: Option<RunningImageEvidence>,
    observation: Option<Arc<RegistrationImage>>,
    pub(super) completed: bool,
}

#[derive(Debug)]
pub(crate) struct RegistrationImage {
    runtime: tokio::runtime::Handle,
    probe: crate::provenance::ExecutableIdentityProbe,
    facts: Mutex<Option<Option<ImageFacts>>>,
    ready: std::sync::Condvar,
}

#[derive(Debug, Clone)]
pub(crate) struct ImageFacts {
    pub(crate) pid: Option<u32>,
    pub(crate) path: Option<PathBuf>,
    pub(crate) identity: Option<crate::provenance::SpawnedFileIdentity>,
    pub(crate) start: Option<u64>,
    pub(crate) exec: Arc<ExecOutcome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecState {
    Pending,
    CommittedExec,
    Refused,
}

/// Records exec confirmation for one launch. An old queued registration must
/// not count a replacement process's successful exec as its own.
#[derive(Debug)]
pub(crate) struct ExecOutcome {
    state: Mutex<ExecState>,
    ready: std::sync::Condvar,
}

impl PartialEq for ExecOutcome {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl ExecOutcome {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(ExecState::Pending),
            ready: std::sync::Condvar::new(),
        }
    }

    pub(crate) fn finish(&self, outcome: ExecState) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if *state == ExecState::Pending {
            *state = outcome;
            self.ready.notify_all();
        }
    }

    fn committed(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        while *state == ExecState::Pending {
            state = self.ready.wait(state).unwrap_or_else(|p| p.into_inner());
        }
        *state == ExecState::CommittedExec
    }
}

/// Dropping an unconfirmed child refuses its queued activity, so the activity
/// writer cannot remain blocked waiting for confirmation that will never come.
#[derive(Debug)]
pub(crate) struct ExecOwner(pub(crate) Arc<ExecOutcome>);

impl Drop for ExecOwner {
    fn drop(&mut self) {
        self.0.finish(ExecState::Refused);
    }
}

impl RegistrationImage {
    pub(crate) fn new(probe: crate::provenance::ExecutableIdentityProbe) -> Self {
        Self {
            runtime: tokio::runtime::Handle::current(),
            probe,
            facts: Mutex::new(None),
            ready: std::sync::Condvar::new(),
        }
    }

    pub(crate) fn complete(&self, facts: Option<ImageFacts>) {
        let mut slot = self.facts.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_none() {
            *slot = Some(facts);
            self.ready.notify_all();
        }
    }

    fn evidence(&self) -> Option<RunningImageEvidence> {
        let facts = {
            let mut slot = self.facts.lock().unwrap_or_else(|p| p.into_inner());
            while slot.is_none() {
                slot = self.ready.wait(slot).unwrap_or_else(|p| p.into_inner());
            }
            slot.as_ref().unwrap().clone()
        }?;
        if !facts.exec.committed() {
            return None;
        }
        running_evidence(self.runtime.block_on(self.probe.observe(
            facts.pid,
            facts.path.as_deref(),
            facts.identity,
            facts.start,
        )))
    }
}

fn running_evidence(image: RunningImageAgreement) -> Option<RunningImageEvidence> {
    match image {
        RunningImageAgreement::Match { evidence }
        | RunningImageAgreement::Mismatch {
            running: evidence, ..
        } => Some(evidence),
        _ => None,
    }
}

impl Drop for PendingLifecycle {
    fn drop(&mut self) {
        if !self.completed {
            self.counters.failed.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(kind = %self.kind, "lifecycle activity append failed or dropped");
        }
    }
}

impl ActivityLedger {
    pub(crate) fn lifecycle_failures(&self) -> u64 {
        self.lifecycle_counters.failed.load(Ordering::Relaxed)
    }

    pub(crate) fn lifecycle_digest_skips(&self) -> u64 {
        self.lifecycle_counters
            .digest_skipped
            .load(Ordering::Relaxed)
    }

    pub(crate) fn skip_start_digest(&self) {
        self.lifecycle_counters
            .digest_skipped
            .fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn module_started(
        &self,
        module_id: &str,
        image: RunningImageAgreement,
        requester: Option<Principal>,
    ) {
        // Build identity comes from the daemon's provenance observation, never
        // the module's declared version or build metadata.
        let Some(evidence) = running_evidence(image) else {
            self.skip_start_digest();
            return;
        };
        self.enqueue_lifecycle(
            "subc.module_started",
            Some(module_id),
            requester.unwrap_or_else(|| Principal::Reserved {
                module_id: "subc".into(),
            }),
            serde_json::Map::new(),
            Some(evidence),
        );
    }

    pub(crate) fn module_registered(
        &self,
        id: &str,
        requester: Option<Principal>,
        observation: Arc<RegistrationImage>,
    ) {
        let mut append = self.lifecycle_append(
            "subc.module_started",
            Some(id),
            requester.unwrap_or_else(|| Principal::Reserved {
                module_id: "subc".into(),
            }),
            serde_json::Map::new(),
            None,
        );
        append.lifecycle.as_mut().unwrap().observation = Some(observation);
        let _ = self.sender.try_send(append);
    }

    pub(crate) fn module_transition(&self, kind: &str, module_id: &str, requester: Principal) {
        self.enqueue_lifecycle(
            kind,
            Some(module_id),
            requester,
            serde_json::Map::new(),
            None,
        );
    }

    pub(crate) fn module_rescanned(
        &self,
        added: &[String],
        removed: &[String],
        requester: Principal,
    ) {
        if added.is_empty() && removed.is_empty() {
            return;
        }
        self.enqueue_lifecycle(
            "subc.module_rescanned",
            None,
            requester,
            serde_json::json!({"added": added, "removed": removed})
                .as_object()
                .unwrap()
                .clone(),
            None,
        );
    }

    pub(crate) fn config_changed(
        &self,
        added: &[String],
        removed: &[String],
        changed: &[String],
        requester: Principal,
    ) {
        if added.is_empty() && removed.is_empty() && changed.is_empty() {
            return;
        }
        let mut details = serde_json::Map::new();
        if !added.is_empty() {
            details.insert("added".into(), serde_json::json!(added));
        }
        if !removed.is_empty() {
            details.insert("removed".into(), serde_json::json!(removed));
        }
        if !changed.is_empty() {
            details.insert("changed".into(), serde_json::json!(changed));
        }
        self.enqueue_lifecycle_with_summary(
            "subc.config_changed",
            None,
            requester,
            details,
            None,
            Some("settings changed"),
        );
    }

    fn enqueue_lifecycle(
        &self,
        kind: &str,
        module_id: Option<&str>,
        principal: Principal,
        details: serde_json::Map<String, serde_json::Value>,
        evidence: Option<RunningImageEvidence>,
    ) {
        self.enqueue_lifecycle_with_summary(kind, module_id, principal, details, evidence, None);
    }

    fn enqueue_lifecycle_with_summary(
        &self,
        kind: &str,
        module_id: Option<&str>,
        principal: Principal,
        details: serde_json::Map<String, serde_json::Value>,
        evidence: Option<RunningImageEvidence>,
        summary: Option<&str>,
    ) {
        // try_send is the entire supervision-side append. The writer owns both
        // retained-build comparison and I/O, and shutdown never joins it.
        let mut append = self.lifecycle_append(kind, module_id, principal, details, evidence);
        if let Some(summary) = summary {
            append.request.summary = summary.into();
        }
        let _ = self.sender.try_send(append);
    }

    fn lifecycle_append(
        &self,
        kind: &str,
        module_id: Option<&str>,
        principal: Principal,
        details: serde_json::Map<String, serde_json::Value>,
        evidence: Option<RunningImageEvidence>,
    ) -> Append {
        Append {
            module: "subc".into(),
            request: ActivityRecordRequest {
                kind: kind.into(),
                summary: kind.strip_prefix("subc.").unwrap_or(kind).replace('_', " "),
                subject: module_id.map(|id| ActivitySubject {
                    subject_type: "module".into(),
                    id: id.into(),
                }),
                details: Some(details),
                on: None,
            },
            actor: ActivityActor {
                principal,
                scope_owner: None,
                agent_id: None,
                run_id: None,
                flow_id: None,
                project_root: None,
            },
            at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            reply: None,
            lifecycle: Some(PendingLifecycle {
                counters: self.lifecycle_counters.clone(),
                kind: kind.into(),
                evidence,
                observation: None,
                completed: false,
            }),
        }
    }
}

impl Writer {
    pub(super) fn prepare_lifecycle(
        &self,
        append: &mut Append,
        errors: &AtomicU64,
    ) -> Result<bool, &'static str> {
        if let Some(pending) = &mut append.lifecycle {
            if let Some(observation) = pending.observation.take() {
                // Observe in queue order: an older registration's slow hash
                // must not overwrite a newer registration's build identity.
                pending.evidence = observation.evidence();
                if pending.evidence.is_none() {
                    pending
                        .counters
                        .digest_skipped
                        .fetch_add(1, Ordering::Relaxed);
                    return Ok(false);
                }
            }
        }
        let Some(evidence) = append
            .lifecycle
            .as_ref()
            .and_then(|pending| pending.evidence.as_ref())
        else {
            return Ok(true);
        };
        let evidence = serde_json::to_value(evidence).map_err(|_| ACTIVITY_UNAVAILABLE)?;
        let id = &append.request.subject.as_ref().unwrap().id;
        // Re-scan retained generations rather than caching the last build:
        // retention can remove the previous start during this daemon lifetime.
        let mut latest: Option<ActivityEntry> = None;
        for (file, len) in self.capture_read()?.files {
            for line in BufReader::new(file.take(len)).split(b'\n') {
                let line = line.map_err(|_| ACTIVITY_UNAVAILABLE)?;
                let entry: ActivityEntry = match serde_json::from_slice(&line) {
                    Ok(entry) => entry,
                    Err(_) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                };
                if entry.kind == "subc.module_started"
                    && entry.subject.as_ref().is_some_and(|subject| {
                        subject.subject_type == "module" && &subject.id == id
                    })
                    && latest.as_ref().is_none_or(|old| entry.seq > old.seq)
                {
                    latest = Some(entry);
                }
            }
        }
        let old = latest
            .as_ref()
            .and_then(|entry| entry.details.get("new_evidence"));
        if old == Some(&evidence) {
            return Ok(false);
        }
        append.request.details = Some(
            serde_json::json!({"old_evidence": old, "new_evidence": evidence})
                .as_object()
                .unwrap()
                .clone(),
        );
        Ok(true)
    }
}
