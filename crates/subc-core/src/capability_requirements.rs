use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
    time::{Duration, Instant},
};

use subc_protocol::manifest::{CapabilityDeclarations, CapabilityNeed};
use tokio::sync::Notify;
use tracing::{error, info, warn};

use crate::supervise::ModuleState;

/// A fresh-exec stall must not suppress a real fleet misconfiguration forever.
/// This fixed v1 ceiling gives each starting candidate time to register while
/// ensuring a module that never reaches HELLO eventually stops masking absence.
pub const CAPABILITY_SETTLE_DEADLINE: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapabilityVerdict {
    Provided,
    Pending,
    NeverProvided,
}

impl CapabilityVerdict {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Provided => "provided",
            Self::Pending => "pending",
            Self::NeverProvided => "never_provided",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessEvidence {
    Absent,
    Starting,
    Registered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequirementSeverity {
    Error,
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequirementStatus {
    pub consumer: String,
    pub capability: String,
    pub need: CapabilityNeed,
    pub verdict: CapabilityVerdict,
    pub episode_seq: u64,
    pub config_satisfiable: bool,
    pub runtime_available: bool,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequirementEvent {
    pub severity: RequirementSeverity,
    pub status: RequirementStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DuplicateClaimEvent {
    pub capability: String,
    pub claimants: Vec<String>,
    pub source: DuplicateClaimSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DuplicateClaimSource {
    Hello,
    CatalogUpdate,
}

impl DuplicateClaimSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hello => "hello",
            Self::CatalogUpdate => "catalog_update",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RuntimeModule {
    pub module_id: String,
    pub state: ModuleState,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct RegisteredModule {
    pub module_id: String,
    pub module_version: String,
    pub capabilities: Option<CapabilityDeclarations>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RequirementKey {
    consumer: String,
    capability: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfiguredCandidate {
    enabled: bool,
}

#[derive(Debug, Clone)]
struct CachedManifest {
    declarations: CapabilityDeclarations,
    module_version: String,
    config_generation: u64,
}

#[derive(Debug, Clone)]
struct CandidateDeadline {
    deadline_ms: u64,
}

#[derive(Debug, Default)]
struct RequirementRecord {
    last_verdict: Option<CapabilityVerdict>,
    episode_seq: u64,
}

#[derive(Debug, Default)]
struct EvaluatorState {
    configured: BTreeMap<String, ConfiguredCandidate>,
    reserved_capabilities: BTreeMap<String, String>,
    config_generation: u64,
    cached: BTreeMap<String, CachedManifest>,
    candidates: BTreeMap<String, CandidateDeadline>,
    process: BTreeMap<String, ProcessEvidence>,
    requirements: BTreeMap<RequirementKey, RequirementRecord>,
    statuses: BTreeMap<RequirementKey, RequirementStatus>,
    refused_claimants: BTreeMap<String, BTreeSet<String>>,
}

/// In-memory, configuration-scoped capability requirement evaluator.
///
/// The evaluator stores only bounded daemon-lifetime evidence. The control
/// handler supplies fresh supervisor and registry snapshots whenever a lifecycle
/// transition occurs; tests supply deterministic millisecond timestamps instead
/// of waiting on wall clock time.
pub(crate) struct CapabilityRequirementEvaluator {
    state: Mutex<EvaluatorState>,
    started_at: Instant,
    wake: Notify,
}

impl Default for CapabilityRequirementEvaluator {
    fn default() -> Self {
        Self::new()
    }
}

impl CapabilityRequirementEvaluator {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(EvaluatorState::default()),
            started_at: Instant::now(),
            wake: Notify::new(),
        }
    }

    pub(crate) fn configure(
        &self,
        modules: impl IntoIterator<Item = (String, bool)>,
        reserved_capabilities: BTreeMap<String, String>,
    ) {
        let configured = modules
            .into_iter()
            .map(|(module_id, enabled)| (module_id, ConfiguredCandidate { enabled }))
            .collect::<BTreeMap<_, _>>();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.configured == configured && state.reserved_capabilities == reserved_capabilities {
            return;
        }
        state.config_generation = state.config_generation.wrapping_add(1);
        state.configured = configured;
        state.reserved_capabilities = reserved_capabilities;
        let configured_ids = state.configured.keys().cloned().collect::<BTreeSet<_>>();
        let reserved_ids = state
            .reserved_capabilities
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let config_generation = state.config_generation;
        state
            .cached
            .retain(|module_id, _| configured_ids.contains(module_id));
        state
            .candidates
            .retain(|module_id, _| configured_ids.contains(module_id));
        state
            .process
            .retain(|module_id, _| configured_ids.contains(module_id));
        state
            .refused_claimants
            .retain(|capability, _| reserved_ids.contains(capability));
        for cache in state.cached.values_mut() {
            cache.config_generation = config_generation;
        }
        drop(state);
        self.wake.notify_one();
    }

    /// Record an attested HELLO as the last-known manifest for a configured id.
    /// Returning true means the claim set drifted from its previous cached value.
    pub(crate) fn record_hello(&self, registration: &RegisteredModule) -> bool {
        let declarations = registration
            .capabilities
            .clone()
            .unwrap_or_else(empty_declarations);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.configured.contains_key(&registration.module_id) {
            return false;
        }
        let drifted = state
            .cached
            .get(&registration.module_id)
            .map(|cached| {
                cached.declarations.provides != declarations.provides
                    || cached.module_version != registration.module_version
            })
            .unwrap_or(false);
        let config_generation = state.config_generation;
        state.cached.insert(
            registration.module_id.clone(),
            CachedManifest {
                declarations,
                module_version: registration.module_version.clone(),
                config_generation,
            },
        );
        drop(state);
        self.wake.notify_one();
        drifted
    }

    /// Return a typed reserved-capability conflict for a claimant before it can
    /// enter the catalog. The bound module is always first; refused claimants are
    /// maintained in lexicographic order for deterministic operator output.
    pub(crate) fn reserved_hello_refusals(
        &self,
        module_id: &str,
        capabilities: Option<&CapabilityDeclarations>,
    ) -> Vec<DuplicateClaimEvent> {
        let Some(capabilities) = capabilities else {
            return Vec::new();
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut events = Vec::new();
        for capability in &capabilities.provides {
            let Some(bound_module) = state.reserved_capabilities.get(capability).cloned() else {
                continue;
            };
            if bound_module == module_id {
                continue;
            }
            state
                .refused_claimants
                .entry(capability.clone())
                .or_default()
                .insert(module_id.to_string());
            let mut claimants = Vec::with_capacity(
                1 + state
                    .refused_claimants
                    .get(capability)
                    .map_or(0, BTreeSet::len),
            );
            claimants.push(bound_module);
            claimants.extend(
                state
                    .refused_claimants
                    .get(capability)
                    .into_iter()
                    .flat_map(|claimants| claimants.iter().cloned()),
            );
            events.push(DuplicateClaimEvent {
                capability: capability.clone(),
                claimants,
                source: DuplicateClaimSource::Hello,
            });
        }
        events
    }

    /// Evaluate all currently known requirements with a monotonic daemon clock.
    pub(crate) fn evaluate_now(
        &self,
        runtime: &[RuntimeModule],
        registered: &[RegisteredModule],
    ) -> Vec<RequirementEvent> {
        self.evaluate_at_ms(
            self.started_at
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            runtime,
            registered,
        )
    }

    /// Recompute with a caller-provided monotonic timestamp. Kept crate-visible
    /// so tests can drive the fixed deadline without shrinking production timing.
    pub(crate) fn evaluate_at_ms(
        &self,
        now_ms: u64,
        runtime: &[RuntimeModule],
        registered: &[RegisteredModule],
    ) -> Vec<RequirementEvent> {
        let runtime = runtime
            .iter()
            .map(|module| (module.module_id.clone(), module))
            .collect::<BTreeMap<_, _>>();
        let registered = registered
            .iter()
            .map(|module| (module.module_id.clone(), module))
            .collect::<BTreeMap<_, _>>();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let configured = state
            .configured
            .iter()
            .map(|(module_id, configured)| (module_id.clone(), configured.clone()))
            .collect::<Vec<_>>();
        for (module_id, configured) in &configured {
            let process = if registered.contains_key(module_id) {
                ProcessEvidence::Registered
            } else if !configured.enabled
                || runtime
                    .get(module_id)
                    .map(|module| !module.enabled)
                    .unwrap_or(false)
            {
                ProcessEvidence::Absent
            } else {
                match runtime.get(module_id).map(|module| module.state) {
                    Some(
                        ModuleState::Starting
                        | ModuleState::Running
                        | ModuleState::Restarting
                        | ModuleState::Draining
                        | ModuleState::Unresponsive,
                    ) => ProcessEvidence::Starting,
                    Some(ModuleState::Stopped | ModuleState::Failed | ModuleState::Disabled)
                    | None => ProcessEvidence::Absent,
                }
            };
            let previous = state.process.insert(module_id.clone(), process);
            match process {
                ProcessEvidence::Starting if previous != Some(ProcessEvidence::Starting) => {
                    state.candidates.insert(
                        module_id.clone(),
                        CandidateDeadline {
                            deadline_ms: now_ms.saturating_add(deadline_ms()),
                        },
                    );
                }
                ProcessEvidence::Starting => {}
                ProcessEvidence::Absent | ProcessEvidence::Registered => {
                    state.candidates.remove(module_id);
                }
            }
        }

        let requirements = requirement_declarations(&state, &registered, &runtime);
        let keys = requirements.keys().cloned().collect::<BTreeSet<_>>();
        state.requirements.retain(|key, _| keys.contains(key));
        state.statuses.retain(|key, _| keys.contains(key));
        let expired_detail = expired_candidate_detail(&state, now_ms);
        let mut events = Vec::new();

        for (key, need) in requirements {
            let runtime_available = registered
                .values()
                .any(|module| provides(module.capabilities.as_ref(), &key.capability));
            let config_satisfiable = state.configured.iter().any(|(module_id, configured)| {
                configured.enabled
                    && runtime
                        .get(module_id)
                        .map(|module| module.enabled)
                        .unwrap_or(true)
                    && (registered.get(module_id).is_some_and(|module| {
                        provides(module.capabilities.as_ref(), &key.capability)
                    }) || state
                        .cached
                        .get(module_id)
                        .is_some_and(|cache| provides(Some(&cache.declarations), &key.capability)))
            });
            let verdict = if runtime_available {
                CapabilityVerdict::Provided
            } else if state.candidates.iter().any(|(module_id, candidate)| {
                candidate.deadline_ms > now_ms
                    && candidate_applies(&state, module_id, &key.capability)
            }) {
                CapabilityVerdict::Pending
            } else {
                CapabilityVerdict::NeverProvided
            };
            let record = state.requirements.entry(key.clone()).or_default();
            let transitioned = record.last_verdict != Some(verdict);
            if transitioned && verdict == CapabilityVerdict::NeverProvided {
                record.episode_seq = record.episode_seq.saturating_add(1);
            }
            record.last_verdict = Some(verdict);
            let detail = requirement_detail(&key, need, verdict, &expired_detail);
            let status = RequirementStatus {
                consumer: key.consumer.clone(),
                capability: key.capability.clone(),
                need,
                verdict,
                episode_seq: record.episode_seq,
                config_satisfiable,
                runtime_available,
                detail,
            };
            state.statuses.insert(key, status.clone());
            if transitioned {
                events.push(RequirementEvent {
                    severity: if need == CapabilityNeed::Required
                        && verdict == CapabilityVerdict::NeverProvided
                    {
                        RequirementSeverity::Error
                    } else {
                        RequirementSeverity::Info
                    },
                    status,
                });
            }
        }
        drop(state);
        events
    }

    pub(crate) fn wake_deadline_loop(&self) {
        self.wake.notify_one();
    }

    pub(crate) fn statuses(&self) -> Vec<RequirementStatus> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .statuses
            .values()
            .cloned()
            .collect()
    }

    pub(crate) fn required_problem_detail(&self, module_id: &str) -> Option<String> {
        let problems = self
            .statuses()
            .into_iter()
            .filter(|status| {
                status.consumer == module_id
                    && status.need == CapabilityNeed::Required
                    && status.verdict == CapabilityVerdict::NeverProvided
            })
            .map(|status| {
                format!(
                    "requires:{} unprovided ({})",
                    status.capability, status.detail
                )
            })
            .collect::<Vec<_>>();
        (!problems.is_empty()).then(|| problems.join("; "))
    }

    pub(crate) fn duplicate_claims(
        &self,
        source: DuplicateClaimSource,
        registered: &[RegisteredModule],
    ) -> Vec<DuplicateClaimEvent> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .reserved_capabilities
            .iter()
            .filter_map(|(capability, bound_module)| {
                let refused = state
                    .refused_claimants
                    .get(capability)
                    .into_iter()
                    .flat_map(|ids| ids.iter());
                let registered_conflicts = registered
                    .iter()
                    .filter(|module| {
                        module.module_id != *bound_module
                            && provides(module.capabilities.as_ref(), capability)
                    })
                    .map(|module| &module.module_id);
                let conflicts = refused
                    .chain(registered_conflicts)
                    .cloned()
                    .collect::<BTreeSet<_>>();
                (!conflicts.is_empty()).then(|| DuplicateClaimEvent {
                    capability: capability.clone(),
                    claimants: std::iter::once(bound_module.clone())
                        .chain(conflicts)
                        .collect(),
                    source,
                })
            })
            .collect()
    }

    /// Evaluate a rescan preview against its resulting enabled module set without
    /// mutating caches, episode counters, tombstones, or deadline state.
    pub(crate) fn preview_removal_warnings(
        &self,
        resulting_modules: impl IntoIterator<Item = (String, bool)>,
        removed: &[String],
        registered: &[RegisteredModule],
    ) -> Vec<String> {
        let enabled = resulting_modules
            .into_iter()
            .filter_map(|(module_id, enabled)| enabled.then_some(module_id))
            .collect::<BTreeSet<_>>();
        let removed = removed.iter().cloned().collect::<BTreeSet<_>>();
        let provided_claims = registered
            .iter()
            .filter(|module| enabled.contains(&module.module_id))
            .flat_map(|module| {
                module
                    .capabilities
                    .as_ref()
                    .into_iter()
                    .flat_map(move |declarations| {
                        declarations
                            .provides
                            .iter()
                            .map(move |capability| (module.module_id.as_str(), capability.as_str()))
                    })
            })
            .collect::<BTreeSet<_>>();
        let mut warnings = BTreeSet::new();
        for consumer in registered
            .iter()
            .filter(|module| enabled.contains(&module.module_id))
        {
            let Some(declarations) = &consumer.capabilities else {
                continue;
            };
            for requirement in declarations
                .requires
                .iter()
                .filter(|requirement| requirement.need == CapabilityNeed::Required)
            {
                if provided_claims
                    .iter()
                    .any(|(_, capability)| *capability == requirement.capability)
                {
                    continue;
                }
                for removed_module in registered.iter().filter(|module| {
                    removed.contains(&module.module_id)
                        && provides(module.capabilities.as_ref(), &requirement.capability)
                }) {
                    warnings.insert(format!(
                        "removing {} leaves {} requires:{} unprovided",
                        removed_module.module_id, consumer.module_id, requirement.capability
                    ));
                }
            }
        }
        warnings.into_iter().collect()
    }

    pub(crate) fn next_deadline_delay(&self) -> Option<Duration> {
        let now_ms: u64 = self
            .started_at
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .candidates
            .values()
            .filter(|candidate| candidate.deadline_ms > now_ms)
            .map(|candidate| Duration::from_millis(candidate.deadline_ms - now_ms))
            .min()
    }

    pub(crate) async fn wait_for_change_or_deadline(&self) {
        let wait = self
            .next_deadline_delay()
            .unwrap_or_else(|| Duration::from_secs(60 * 60));
        tokio::select! {
            _ = self.wake.notified() => {}
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

pub(crate) fn log_requirement_events(events: impl IntoIterator<Item = RequirementEvent>) {
    for event in events {
        let status = event.status;
        match event.severity {
            RequirementSeverity::Error => error!(
                consumer = %status.consumer,
                capability = %status.capability,
                need = need_name(status.need),
                verdict = status.verdict.as_str(),
                episode_seq = status.episode_seq,
                config_satisfiable = status.config_satisfiable,
                runtime_available = status.runtime_available,
                detail = %status.detail,
                "capability.requirement"
            ),
            RequirementSeverity::Info => info!(
                consumer = %status.consumer,
                capability = %status.capability,
                need = need_name(status.need),
                verdict = status.verdict.as_str(),
                episode_seq = status.episode_seq,
                config_satisfiable = status.config_satisfiable,
                runtime_available = status.runtime_available,
                detail = %status.detail,
                "capability.requirement"
            ),
        }
    }
}

pub(crate) fn log_duplicate_claim_events(events: impl IntoIterator<Item = DuplicateClaimEvent>) {
    for event in events {
        warn!(
            capability = %event.capability,
            claimants = ?event.claimants,
            source = event.source.as_str(),
            "capability.duplicate_claim"
        );
    }
}

fn requirement_declarations(
    state: &EvaluatorState,
    registered: &BTreeMap<String, &RegisteredModule>,
    runtime: &BTreeMap<String, &RuntimeModule>,
) -> BTreeMap<RequirementKey, CapabilityNeed> {
    let mut declarations = BTreeMap::new();
    for (module_id, configured) in &state.configured {
        if !configured.enabled
            || runtime
                .get(module_id)
                .map(|module| !module.enabled)
                .unwrap_or(false)
        {
            continue;
        }
        if let Some(cache) = state.cached.get(module_id) {
            insert_requirements(&mut declarations, module_id, &cache.declarations);
        }
    }
    for (module_id, module) in registered {
        insert_requirements(
            &mut declarations,
            module_id,
            module
                .capabilities
                .as_ref()
                .unwrap_or(&empty_declarations()),
        );
    }
    declarations
}

fn insert_requirements(
    output: &mut BTreeMap<RequirementKey, CapabilityNeed>,
    consumer: &str,
    declarations: &CapabilityDeclarations,
) {
    for requirement in &declarations.requires {
        output.insert(
            RequirementKey {
                consumer: consumer.to_string(),
                capability: requirement.capability.clone(),
            },
            requirement.need,
        );
    }
}

fn candidate_applies(state: &EvaluatorState, module_id: &str, capability: &str) -> bool {
    match state.cached.get(module_id) {
        Some(cache) => provides(Some(&cache.declarations), capability),
        None => true,
    }
}

fn expired_candidate_detail(state: &EvaluatorState, now_ms: u64) -> String {
    state
        .candidates
        .iter()
        .filter(|(module_id, candidate)| {
            candidate.deadline_ms <= now_ms
                && state.process.get(*module_id) == Some(&ProcessEvidence::Starting)
        })
        .map(|(module_id, _)| match state.cached.get(module_id) {
            None => format!("{module_id} still starting after 120s (claims unknown)"),
            Some(cache) => {
                let claims = cache
                    .declarations
                    .provides
                    .iter()
                    .cloned()
                    .collect::<BTreeSet<_>>();
                format!(
                    "{module_id} still starting after 120s (claims cached: {})",
                    claims.into_iter().collect::<Vec<_>>().join(", ")
                )
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn requirement_detail(
    key: &RequirementKey,
    need: CapabilityNeed,
    verdict: CapabilityVerdict,
    expired_detail: &str,
) -> String {
    let base = format!(
        "{} requires:{} {} ({})",
        key.consumer,
        key.capability,
        verdict.as_str(),
        need_name(need)
    );
    if expired_detail.is_empty() {
        base
    } else {
        format!("{base}; {expired_detail}")
    }
}

fn provides(declarations: Option<&CapabilityDeclarations>, capability: &str) -> bool {
    declarations.is_some_and(|declarations| {
        declarations
            .provides
            .iter()
            .any(|claim| claim == capability)
    })
}

fn need_name(need: CapabilityNeed) -> &'static str {
    match need {
        CapabilityNeed::Required => "required",
        CapabilityNeed::Optional => "optional",
    }
}

fn empty_declarations() -> CapabilityDeclarations {
    CapabilityDeclarations {
        provides: Vec::new(),
        requires: Vec::new(),
        must_never_reach: Vec::new(),
    }
}

fn deadline_ms() -> u64 {
    CAPABILITY_SETTLE_DEADLINE
        .as_millis()
        .try_into()
        .expect("120 second deadline fits u64 milliseconds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use subc_protocol::manifest::CapabilityRequirement;

    fn declarations(
        provides: &[&str],
        requires: &[(&str, CapabilityNeed)],
    ) -> CapabilityDeclarations {
        CapabilityDeclarations {
            provides: provides.iter().map(|value| (*value).to_string()).collect(),
            requires: requires
                .iter()
                .map(|(capability, need)| CapabilityRequirement {
                    capability: (*capability).to_string(),
                    need: *need,
                })
                .collect(),
            must_never_reach: Vec::new(),
        }
    }

    fn registered(
        module_id: &str,
        provides: &[&str],
        requires: &[(&str, CapabilityNeed)],
    ) -> RegisteredModule {
        RegisteredModule {
            module_id: module_id.to_string(),
            module_version: "1.0.0".to_string(),
            capabilities: Some(declarations(provides, requires)),
        }
    }

    fn runtime(module_id: &str, state: ModuleState) -> RuntimeModule {
        RuntimeModule {
            module_id: module_id.to_string(),
            state,
            enabled: true,
        }
    }

    #[test]
    fn verdict_computation_mutation_proof_cold_boot_rearms_each_absence_episode() {
        let evaluator = CapabilityRequirementEvaluator::new();
        evaluator.configure(
            [
                ("consumer".to_string(), true),
                ("provider".to_string(), true),
            ],
            BTreeMap::new(),
        );
        let consumer = registered(
            "consumer",
            &[],
            &[("credentials-provider/v1", CapabilityNeed::Required)],
        );
        let pending = evaluator.evaluate_at_ms(
            0,
            &[
                runtime("consumer", ModuleState::Running),
                runtime("provider", ModuleState::Starting),
            ],
            std::slice::from_ref(&consumer),
        );
        assert_eq!(pending[0].status.verdict, CapabilityVerdict::Pending);
        assert_eq!(pending[0].status.episode_seq, 0);

        let missing = evaluator.evaluate_at_ms(
            deadline_ms(),
            &[
                runtime("consumer", ModuleState::Running),
                runtime("provider", ModuleState::Starting),
            ],
            std::slice::from_ref(&consumer),
        );
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].severity, RequirementSeverity::Error);
        assert_eq!(missing[0].status.verdict, CapabilityVerdict::NeverProvided);
        assert_eq!(missing[0].status.episode_seq, 1);

        let provider = registered("provider", &["credentials-provider/v1"], &[]);
        let restored = evaluator.evaluate_at_ms(
            deadline_ms() + 1,
            &[
                runtime("consumer", ModuleState::Running),
                runtime("provider", ModuleState::Running),
            ],
            &[consumer.clone(), provider],
        );
        assert_eq!(restored[0].status.verdict, CapabilityVerdict::Provided);

        let rebroken = evaluator.evaluate_at_ms(
            deadline_ms() + 2,
            &[
                runtime("consumer", ModuleState::Running),
                runtime("provider", ModuleState::Starting),
            ],
            std::slice::from_ref(&consumer),
        );
        assert_eq!(rebroken[0].status.verdict, CapabilityVerdict::Pending);
        let second_absence = evaluator.evaluate_at_ms(
            deadline_ms().saturating_mul(2) + 2,
            &[
                runtime("consumer", ModuleState::Running),
                runtime("provider", ModuleState::Starting),
            ],
            &[consumer],
        );
        assert_eq!(second_absence[0].status.episode_seq, 2);
    }

    #[test]
    fn deadline_expiry_is_per_candidate_and_does_not_extend_earlier_candidate() {
        let evaluator = CapabilityRequirementEvaluator::new();
        evaluator.configure(
            [
                ("consumer".to_string(), true),
                ("first".to_string(), true),
                ("second".to_string(), true),
            ],
            BTreeMap::new(),
        );
        let consumer = registered(
            "consumer",
            &[],
            &[("credentials-provider/v1", CapabilityNeed::Required)],
        );
        evaluator.evaluate_at_ms(
            0,
            &[
                runtime("consumer", ModuleState::Running),
                runtime("first", ModuleState::Starting),
            ],
            std::slice::from_ref(&consumer),
        );
        evaluator.evaluate_at_ms(
            60_000,
            &[
                runtime("consumer", ModuleState::Running),
                runtime("first", ModuleState::Starting),
                runtime("second", ModuleState::Starting),
            ],
            std::slice::from_ref(&consumer),
        );
        let first_expired = evaluator.evaluate_at_ms(
            deadline_ms(),
            &[
                runtime("consumer", ModuleState::Running),
                runtime("first", ModuleState::Starting),
                runtime("second", ModuleState::Starting),
            ],
            std::slice::from_ref(&consumer),
        );
        assert!(
            first_expired.is_empty(),
            "the second candidate still suppresses"
        );
        assert_eq!(evaluator.statuses()[0].verdict, CapabilityVerdict::Pending);
        assert!(evaluator.statuses()[0]
            .detail
            .contains("first still starting after 120s (claims unknown)"));

        let both_expired = evaluator.evaluate_at_ms(
            180_000,
            &[
                runtime("consumer", ModuleState::Running),
                runtime("first", ModuleState::Starting),
                runtime("second", ModuleState::Starting),
            ],
            &[consumer],
        );
        assert_eq!(
            both_expired[0].status.verdict,
            CapabilityVerdict::NeverProvided
        );
    }

    #[test]
    fn cached_evidence_suppresses_only_its_cached_set_and_renders_in_order() {
        let evaluator = CapabilityRequirementEvaluator::new();
        evaluator.configure(
            [
                ("consumer".to_string(), true),
                ("cached".to_string(), true),
                ("unknown".to_string(), true),
            ],
            BTreeMap::new(),
        );
        evaluator.record_hello(&registered("cached", &["z/v1", "a/v1"], &[]));
        let consumer = registered(
            "consumer",
            &[],
            &[
                ("a/v1", CapabilityNeed::Required),
                ("other/v1", CapabilityNeed::Required),
            ],
        );
        evaluator.evaluate_at_ms(
            0,
            &[
                runtime("consumer", ModuleState::Running),
                runtime("cached", ModuleState::Starting),
                runtime("unknown", ModuleState::Starting),
            ],
            std::slice::from_ref(&consumer),
        );
        let expired = evaluator.evaluate_at_ms(
            deadline_ms(),
            &[
                runtime("consumer", ModuleState::Running),
                runtime("cached", ModuleState::Starting),
                runtime("unknown", ModuleState::Starting),
            ],
            &[consumer],
        );
        assert_eq!(expired.len(), 2);
        let detail = &expired[0].status.detail;
        assert!(detail.contains("cached still starting after 120s (claims cached: a/v1, z/v1)"));
        assert!(detail.contains("unknown still starting after 120s (claims unknown)"));
        assert!(detail.find("cached still").unwrap() < detail.find("unknown still").unwrap());
    }

    #[test]
    fn cached_candidate_suppresses_only_cached_capabilities_before_its_deadline() {
        let evaluator = CapabilityRequirementEvaluator::new();
        evaluator.configure(
            [("consumer".to_string(), true), ("cached".to_string(), true)],
            BTreeMap::new(),
        );
        evaluator.record_hello(&registered("cached", &["a/v1"], &[]));
        let consumer = registered(
            "consumer",
            &[],
            &[
                ("a/v1", CapabilityNeed::Required),
                ("b/v1", CapabilityNeed::Required),
            ],
        );
        let events = evaluator.evaluate_at_ms(
            0,
            &[
                runtime("consumer", ModuleState::Running),
                runtime("cached", ModuleState::Starting),
            ],
            &[consumer],
        );
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].status.capability, "a/v1");
        assert_eq!(events[0].status.verdict, CapabilityVerdict::Pending);
        assert_eq!(events[1].status.capability, "b/v1");
        assert_eq!(events[1].status.verdict, CapabilityVerdict::NeverProvided);
    }

    #[test]
    fn disabled_candidate_is_absent_and_never_suppresses() {
        let evaluator = CapabilityRequirementEvaluator::new();
        evaluator.configure(
            [
                ("consumer".to_string(), true),
                ("provider".to_string(), false),
            ],
            BTreeMap::new(),
        );
        let consumer = registered(
            "consumer",
            &[],
            &[("credentials-provider/v1", CapabilityNeed::Required)],
        );
        let events =
            evaluator.evaluate_at_ms(0, &[runtime("consumer", ModuleState::Running)], &[consumer]);
        assert_eq!(events[0].status.verdict, CapabilityVerdict::NeverProvided);
        assert!(!events[0].status.config_satisfiable);
    }

    #[test]
    fn duplicate_claim_event_orders_bound_then_refused_for_both_sources() {
        let evaluator = CapabilityRequirementEvaluator::new();
        evaluator.configure(
            [],
            BTreeMap::from([("credentials-provider/v1".to_string(), "vault".to_string())]),
        );
        let hello = evaluator.reserved_hello_refusals(
            "zeta",
            Some(&declarations(&["credentials-provider/v1"], &[])),
        );
        assert_eq!(hello[0].claimants, ["vault", "zeta"]);
        let update = evaluator.duplicate_claims(
            DuplicateClaimSource::CatalogUpdate,
            &[registered("alpha", &["credentials-provider/v1"], &[])],
        );
        assert_eq!(update[0].source, DuplicateClaimSource::CatalogUpdate);
        assert_eq!(update[0].claimants, ["vault", "alpha", "zeta"]);
    }
}
