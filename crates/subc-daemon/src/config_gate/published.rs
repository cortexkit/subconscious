//! Admission, status and approved daemon configuration share one publication.
use super::*;
use std::sync::Arc;
use subc_protocol::peer::SettingsCredentials;

pub(crate) struct Published {
    pub(super) admission: admission::AdmissionCache,
    pub(crate) approved_daemon: ApprovedDaemonView,
    pub(super) projection: polled::Projection,
    pub(super) settings: Option<settings::SettingsVersion>,
    pub(super) settings_credentials: Option<Arc<ApprovedSettingsCredentials>>,
}

pub(super) struct ApprovedSettingsCredentials {
    revision: u64,
    modules: BTreeMap<String, ModuleSettingsCredentials>,
}

#[derive(Default)]
struct ModuleSettingsCredentials {
    user: Option<SettingsCredentials>,
    projects: BTreeMap<String, Option<SettingsCredentials>>,
}

impl ApprovedSettingsCredentials {
    pub(super) fn from_store(
        store: &ConfigStore,
        recovering: bool,
        previous: Option<&Arc<Self>>,
    ) -> Option<Arc<Self>> {
        let (state, body) = store.loaded_admission_state();
        if state != StoreState::Stamped || recovering || store.loaded_anchor_pending() {
            return None;
        }
        let revision = body.last_generation;
        if let Some(previous) = previous.filter(|previous| previous.revision == revision) {
            return Some(previous.clone());
        }

        // The committed settings authorize these refs even if the module has
        // not yet applied that revision. Settings proposed for approval do not.
        let mut modules = BTreeMap::<_, ModuleSettingsCredentials>::new();
        let mut schemas = BTreeMap::new();
        for (tuple, _) in &body.versions {
            let module = tuple.module.as_str();
            let entry = match tuple.project.as_ref() {
                Some(_) => settings::projects::approved_project(body, tuple),
                None => settings::approved_entry(body, module),
            };
            let Some(entry) = entry else { continue };
            let credentials = (|| {
                let delivery = entry.delivery.as_ref()?;
                let view = match tuple.project.as_ref() {
                    Some(_) => settings::projects::project_view(entry, tuple),
                    None => delivery.modules.get(module),
                }?;
                let generation = entry.generation.id;
                let schema = schemas
                    .entry((module, generation))
                    .or_insert_with(|| settings::schema(delivery.declarations.get(module)?).ok())
                    .as_ref()?;
                SettingsCredentials::new(generation, schema.credential_refs(&view.effective)).ok()
            })();
            let projected = modules.entry(tuple.module.clone()).or_default();
            match &tuple.project {
                Some(project) => {
                    // Only fall back to user when there is no project approval,
                    // not when those approved values could not be attested.
                    projected.projects.insert(project.clone(), credentials);
                }
                None => projected.user = credentials,
            }
        }
        Some(Arc::new(Self { revision, modules }))
    }

    fn get(&self, module: &str, project: Option<&str>) -> Option<&SettingsCredentials> {
        let module = self.modules.get(module)?;
        project
            .and_then(|project| module.projects.get(project))
            .unwrap_or(&module.user)
            .as_ref()
    }
}

impl Published {
    pub(crate) fn registration_declaration(&self, module: &str) -> Option<String> {
        self.admission
            .registration_declarations()?
            .get(module)
            .cloned()
    }
    pub(crate) fn is_recovering(&self) -> bool {
        self.admission.is_recovering()
    }
}

impl std::ops::Deref for Published {
    type Target = admission::AdmissionCache;
    fn deref(&self) -> &Self::Target {
        &self.admission
    }
}

impl RuntimeOwner {
    pub(crate) fn settings_credentials(
        &self,
        module: &str,
        project: Option<&str>,
    ) -> Option<SettingsCredentials> {
        self.polled_snapshot()
            .ok()?
            .settings_credentials
            .as_ref()?
            .get(module, project)
            .cloned()
    }

    #[cfg(all(test, unix, feature = "test-support"))]
    pub(crate) fn set_polled_capture_hook(&self, hook: impl FnOnce() + Send + 'static) {
        *self.polled_capture_hook.lock().unwrap() = Some(Box::new(hook));
    }

    pub(crate) fn polled_snapshot(&self) -> Result<std::sync::Arc<Published>> {
        #[cfg(test)]
        self.polled_capture_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if self.inner.is_poisoned() {
            return Err(GateError::new(
                "config_unavailable",
                "configuration owner lock is poisoned",
            ));
        }
        let snapshot = self
            .published
            .read()
            .map(|value| value.clone())
            .map_err(|_| {
                GateError::new(
                    "config_unavailable",
                    "configuration publication is unavailable",
                )
            })?;
        #[cfg(test)]
        if let Some(hook) = self.polled_capture_hook.lock().unwrap().take() {
            hook();
        }
        Ok(snapshot)
    }
}
