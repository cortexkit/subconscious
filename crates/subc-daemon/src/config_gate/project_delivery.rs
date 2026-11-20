//! Stamped project delivery uses stored approval results; it never opens a layer file.

use super::*;
use crate::config_store::{HistoryEntry, StoreBody, Versions};
use subc_protocol::config::{ConfigChanged, ConfigDeclarationPending};

#[derive(Default)]
pub(super) struct ProjectSlot {
    // A manual get or a discovery push subscribes this connection to the project.
    announced: Option<ProjectNotice>,
    attempt: Option<Attempt>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProjectNotice {
    Active(u64),
    Pending(u64),
}

impl ProjectNotice {
    fn generation(self) -> u64 {
        match self {
            Self::Active(generation) | Self::Pending(generation) => generation,
        }
    }
}

#[derive(Default)]
pub(crate) struct ProjectDeliveryPass {
    pub hints: Vec<ConfigChanged>,
    pub windows: Vec<(String, DeliveryWindow)>,
}

impl DeliveryState {
    pub(super) fn attempt_for(
        &self,
        connection: ConnectionId,
        project: Option<&str>,
    ) -> Option<&Attempt> {
        match project {
            Some(project) => self
                .projects
                .get(&connection)?
                .get(project)?
                .attempt
                .as_ref(),
            None => self.attempts.get(&connection),
        }
    }

    pub(super) fn attempt_for_mut(
        &mut self,
        connection: ConnectionId,
        project: Option<&str>,
    ) -> Option<&mut Attempt> {
        match project {
            Some(project) => self
                .projects
                .get_mut(&connection)?
                .get_mut(project)?
                .attempt
                .as_mut(),
            None => self.attempts.get_mut(&connection),
        }
    }
}

fn tuple(module: &str, project: &str) -> Tuple {
    Tuple {
        module: module.into(),
        project: Some(project.into()),
    }
}

fn registered(body: &StoreBody, project: &str, own: bool) -> Result<()> {
    match body.registry_membership.as_ref() {
        Some(membership) if !membership.projects.contains_key(project) => Err(GateError::new(
            "unknown_project",
            "project is not registered",
        )),
        None if !own => Err(GateError::new(
            "project_registry_unavailable",
            "no confirmed project list is available",
        )),
        _ => Ok(()),
    }
}

fn declaration_matches(
    entry: &HistoryEntry,
    module: &str,
    declaration: &ConfigDeclareRequest,
) -> bool {
    entry
        .delivery
        .as_ref()
        .and_then(|delivery| delivery.declarations.get(module))
        == Some(declaration)
}

fn pending_for(
    body: &StoreBody,
    key: &Tuple,
    versions: Option<&Versions>,
    declaration: &ConfigDeclareRequest,
) -> Option<u64> {
    versions?.pending.filter(|generation| {
        body.project_sendable(key, *generation)
            && body
                .generations
                .get(generation)
                .is_some_and(|entry| declaration_matches(entry, &key.module, declaration))
    })
}

fn active_user(body: &StoreBody, module: &str) -> Option<u64> {
    body.user_versions(module)?.active
}

fn notice_for(
    body: &StoreBody,
    key: &Tuple,
    versions: Option<&Versions>,
    declaration: &ConfigDeclareRequest,
    requested: bool,
) -> Option<ProjectNotice> {
    pending_for(body, key, versions, declaration)
        .map(ProjectNotice::Pending)
        .or_else(|| {
            versions
                .and_then(|v| v.active)
                .or_else(|| requested.then(|| active_user(body, &key.module)).flatten())
                .map(ProjectNotice::Active)
        })
}

fn hint_notice(
    state: &OwnerState,
    connection: ConnectionId,
    module: &str,
    project: &str,
) -> Option<ProjectNotice> {
    let (status, body) = state.store.loaded_admission_state();
    if status != crate::config_store::StoreState::Stamped
        || state.store.loaded_anchor_pending()
        || state.active_declarations.get(module) != Some(&connection)
        || registered(body, project, false).is_err()
    {
        return None;
    }
    let key = tuple(module, project);
    let requested = state
        .delivery
        .projects
        .get(&connection)
        .is_some_and(|slots| slots.contains_key(project));
    notice_for(
        body,
        &key,
        body.project_versions(&key),
        &state.declarations.get(&connection)?.1,
        requested,
    )
}

fn load_view(
    body: &StoreBody,
    key: &Tuple,
    generation: u64,
    own: bool,
    status: ConfigStatus,
) -> Result<ConfigView> {
    let mut view = body
        .generations
        .get(&generation)
        .and_then(|entry| entry.delivery.as_ref())
        .and_then(|delivery| {
            if own {
                delivery
                    .project_views
                    .get(&key.module)?
                    .get(key.project.as_deref()?)
            } else {
                delivery.modules.get(&key.module)
            }
        })
        .cloned()
        .ok_or_else(|| GateError::new("config_gated", "approved project view is absent"))?;
    view.generation = generation;
    view.project_id.clone_from(&key.project);
    view.status = status;
    view.attempt = None;
    Ok(view)
}

impl RuntimeOwner {
    pub(crate) fn project_settings_enabled(&self) -> bool {
        self.project_capture_enabled
    }

    pub(crate) async fn refresh_project_membership(self: &std::sync::Arc<Self>) -> Result<()> {
        let before = self
            .on_blocking(|owner| {
                let guard = owner.lock()?;
                let (state, body) = guard.store.loaded_admission_state();
                Ok((state == crate::config_store::StoreState::Stamped)
                    .then(|| body.registry_membership.clone()))
            })
            .await?;
        let Some(before) = before else { return Ok(()) };
        let (reply, _) = self
            .capturer
            .projects
            .capture_batch(&BTreeMap::new())
            .await?;
        let Some(reply) = reply else { return Ok(()) };
        self.on_blocking(move |owner| {
            let mut guard = owner.lock()?;
            let (state, body) = guard.store.loaded_admission_state();
            // A capture or apply can publish a newer observed membership while
            // the poll RPC is in flight. Leave that observation in place.
            if state == crate::config_store::StoreState::Stamped
                && body.registry_membership == before
                && guard.store.replace_registry_membership(Ok(&reply))?
            {
                owner.notify_delivery_changed();
            }
            Ok(())
        })
        .await
    }

    pub(in crate::config_gate::runtime) fn stamped_project_get(
        &self,
        state: &mut OwnerState,
        connection: ConnectionId,
        module: &str,
        declaration: &ConfigDeclareRequest,
        request: &ConfigGetRequest,
        assign: bool,
    ) -> Result<ConfigGetReply> {
        let project = request
            .project_id
            .as_deref()
            .expect("project get has an id");
        let body = if assign {
            state.store.delivery_ready()?;
            state.store.body()?
        } else {
            state.store.loaded_admission_state().1
        };
        let key = tuple(module, project);
        let versions = body.project_versions(&key);
        let own = versions.and_then(|v| v.active);
        registered(
            body,
            project,
            own.is_some() || versions.is_some_and(|v| v.pending.is_some()),
        )?;
        let user = body.user_versions(module);
        let active = own.or_else(|| user.and_then(|v| v.active));
        let active_target = self.project_capture_enabled
            && state.active_declarations.get(module) == Some(&connection);
        let pending = active_target
            .then(|| pending_for(body, &key, versions, declaration))
            .flatten();
        let selected = pending
            .map(ProjectNotice::Pending)
            .or(active.map(ProjectNotice::Active))
            .ok_or_else(|| {
                GateError::new(
                    "config_gated",
                    "module has no approved active or deliverable project configuration",
                )
            })?;
        let bound = state.delivery.attempt_for(connection, Some(project));
        match (request.generation, request.attempt) {
            (None, None) => {}
            (Some(generation), None) if pending.is_none() && active == Some(generation) => {}
            (Some(generation), Some(attempt))
                if pending == Some(generation)
                    && bound.is_some_and(|b| b.generation == generation && b.id == attempt) => {}
            _ => {
                return Err(GateError::new(
                    "config_gated",
                    "requested project delivery is not assigned to this connection",
                ))
            }
        }
        let mut reply = ConfigGetReply::new(
            active
                .map(|id| load_view(body, &key, id, own.is_some(), ConfigStatus::Active))
                .transpose()?,
            pending
                .map(|id| load_view(body, &key, id, true, ConfigStatus::Pending))
                .transpose()?,
        );
        let approval = versions
            .and_then(|v| v.pending.or(v.active))
            .or_else(|| user.and_then(|v| v.pending.or(v.active)));
        if approval
            .and_then(|id| body.generations.get(&id))
            .is_none_or(|entry| !declaration_matches(entry, module, declaration))
        {
            let mut marker = ConfigDeclarationPending::default();
            marker.plan_digest = state
                .gate
                .plans
                .values()
                .filter(|plan| {
                    plan.delivery.as_ref().is_some_and(|delivery| {
                        delivery.declarations.get(module) == Some(declaration)
                            && (own.is_none()
                                || delivery
                                    .project_views
                                    .get(module)
                                    .is_some_and(|views| views.contains_key(project)))
                    })
                })
                .max_by_key(|plan| plan.creation_sequence)
                .map(|plan| plan.view.digest.clone());
            reply.declaration_pending = Some(marker);
        }
        if assign && self.project_capture_enabled {
            let replacement = pending
                .filter(|id| bound.is_none_or(|b| b.generation != *id))
                .map(|id| state.delivery.allocate(module, id, Transition::Live))
                .transpose()?;
            let slot = state
                .delivery
                .projects
                .entry(connection)
                .or_default()
                .entry(project.into())
                .or_default();
            slot.announced = Some(selected);
            if pending.is_none() || replacement.is_some() {
                slot.attempt = replacement;
            }
            if let Some(view) = &mut reply.pending {
                view.attempt = slot.attempt.as_ref().map(|attempt| attempt.id);
            }
        }
        Ok(reply)
    }

    pub(super) fn project_view_delivered(
        state: &mut OwnerState,
        connection: ConnectionId,
        module: &str,
        project: &str,
        reply: &ConfigGetReply,
        written_at: tokio::time::Instant,
        active: bool,
    ) -> Result<bool> {
        let body = state.store.loaded_admission_state().1;
        let key = tuple(module, project);
        let versions = body.project_versions(&key);
        let active_generation = versions
            .and_then(|v| v.active)
            .or_else(|| body.user_versions(module).and_then(|v| v.active));
        let superseded = || {
            GateError::new(
                "delivery_superseded",
                "project configuration changed while its view was being written",
            )
        };
        if registered(body, project, versions.is_some()).is_err()
            || reply
                .active
                .as_ref()
                .is_some_and(|view| Some(view.generation) != active_generation)
        {
            return Err(superseded());
        }
        if let Some(view) = reply.pending.as_ref().filter(|_| active) {
            if pending_for(body, &key, versions, &state.declarations[&connection].1)
                != Some(view.generation)
            {
                return Err(superseded());
            }
            return state
                .delivery
                .attempt_for_mut(connection, Some(project))
                .filter(|bound| {
                    bound.generation == view.generation && Some(bound.id) == view.attempt
                })
                .ok_or_else(superseded)?
                .written(written_at);
        }
        Ok(false)
    }

    pub(super) fn project_outcome(
        &self,
        state: &mut OwnerState,
        connection: ConnectionId,
        module: &str,
        project: &str,
        request: &ConfigAppliedRequest,
        timed_out: bool,
    ) -> Result<AppliedDisposition> {
        if !self.project_capture_enabled {
            return Err(GateError::new(
                "config_gated",
                "project-tuple delivery is not enabled",
            ));
        }
        let key = tuple(module, project);
        let bound = state
            .delivery
            .attempt_for(connection, Some(project))
            .filter(|bound| {
                bound.module == module
                    && bound.generation == request.generation
                    && bound.id == request.attempt
                    && (bound.delivered || timed_out)
            });
        let Some(bound) = bound else {
            let body = state.store.loaded_admission_state().1;
            if !request.result.is_ack()
                && registered(body, project, false).is_ok()
                && body
                    .project_versions(&key)
                    .is_none_or(|v| v.active.is_none() && v.pending.is_none())
            {
                tracing::warn!(
                    module_id = module,
                    project_id = project,
                    "ignored rejection of project fallback"
                );
            }
            return Ok(AppliedDisposition::Ignored);
        };
        if state.active_declarations.get(module) != Some(&connection) {
            return Err(GateError::new(
                "not_delivery_target",
                "connection is not the active delivery target",
            ));
        }
        let ack = request.result.is_ack()
            && bound
                .deadline
                .is_none_or(|deadline| tokio::time::Instant::now() < deadline);
        let body = state.store.body()?;
        let versions = body.project_versions(&key);
        let restore = versions.and_then(|v| v.active);
        let visible = restore.or_else(|| active_user(body, module));
        if registered(body, project, versions.is_some()).is_err()
            || pending_for(body, &key, versions, &state.declarations[&connection].1)
                != Some(request.generation)
        {
            return Ok(AppliedDisposition::Ignored);
        }
        let result = state.store.record_delivery(&key, request.generation, ack);
        if result.is_ok() || !ack {
            if let Some(slot) = state
                .delivery
                .projects
                .get_mut(&connection)
                .and_then(|slots| slots.get_mut(project))
            {
                slot.attempt = None;
                slot.announced = if ack {
                    Some(ProjectNotice::Active(request.generation))
                } else {
                    visible.map(ProjectNotice::Active)
                };
            }
        }
        self.notify_delivery_changed();
        result?;
        Ok(if ack {
            AppliedDisposition::Active
        } else {
            AppliedDisposition::Failed {
                generation: request.generation,
                restore,
                keep_running: true,
            }
        })
    }

    pub(crate) fn project_delivery_pass(
        &self,
        connection: ConnectionId,
        module: &str,
    ) -> Result<ProjectDeliveryPass> {
        let mut pass = ProjectDeliveryPass::default();
        if !self.project_capture_enabled {
            return Ok(pass);
        }
        let mut guard = self.lock()?;
        if guard.active_declarations.get(module) != Some(&connection)
            || guard.store.loaded_admission_state().0 != crate::config_store::StoreState::Stamped
            || guard.store.loaded_anchor_pending()
        {
            return Ok(pass);
        }
        let state = &mut *guard;
        let body = state.store.loaded_admission_state().1;
        let declaration = &state.declarations[&connection].1;
        let Some(membership) = &body.registry_membership else {
            return Ok(pass);
        };
        let slots = state.delivery.projects.entry(connection).or_default();
        for (project, slot) in slots.iter_mut() {
            if !membership.projects.contains_key(project) {
                *slot = ProjectSlot::default();
            }
        }
        let own: BTreeMap<_, _> = body
            .versions
            .iter()
            .filter_map(|(key, versions)| {
                let project = key.project.as_ref()?;
                (key.module == module && membership.projects.contains_key(project))
                    .then_some((project, versions))
            })
            .collect();
        let known: BTreeSet<_> = slots.keys().chain(own.keys().copied()).cloned().collect();
        for project in known {
            if !membership.projects.contains_key(&project) {
                continue;
            }
            let slot = slots.get_mut(&project);
            let key = tuple(module, &project);
            let notice = notice_for(
                body,
                &key,
                own.get(&project).copied(),
                declaration,
                slot.is_some(),
            );
            if let Some(notice) = notice
                .filter(|notice| slot.as_ref().and_then(|slot| slot.announced) != Some(*notice))
            {
                let mut hint = ConfigChanged::new(notice.generation(), vec![]);
                hint.project_id = Some(project.clone());
                pass.hints.push(hint);
            }
            if let Some(slot) = slot {
                if let Some(attempt) = slot
                    .attempt
                    .as_ref()
                    .filter(|attempt| notice == Some(ProjectNotice::Pending(attempt.generation)))
                {
                    pass.windows.push((project, attempt.window()));
                } else {
                    // A user transition can hide the pending generation. A
                    // new attempt is needed if it becomes visible again.
                    slot.attempt = None;
                }
            }
        }
        Ok(pass)
    }

    pub(crate) fn project_hint_current(
        &self,
        connection: ConnectionId,
        module: &str,
        project: &str,
        generation: u64,
    ) -> Result<bool> {
        let state = self.lock()?;
        let notice = hint_notice(&state, connection, module, project);
        let announced = state
            .delivery
            .projects
            .get(&connection)
            .and_then(|slots| slots.get(project))
            .and_then(|slot| slot.announced);
        Ok(notice != announced && notice.is_some_and(|notice| notice.generation() == generation))
    }

    pub(crate) fn project_hint_sent(
        &self,
        connection: ConnectionId,
        module: &str,
        project: String,
        generation: u64,
    ) -> Result<()> {
        let mut state = self.lock()?;
        if state.active_declarations.get(module) == Some(&connection) {
            let notice = hint_notice(&state, connection, module, &project)
                .filter(|notice| notice.generation() == generation);
            let slot = state
                .delivery
                .projects
                .entry(connection)
                .or_default()
                .entry(project)
                .or_default();
            if notice.is_some() {
                slot.announced = notice;
            }
        }
        Ok(())
    }
}
