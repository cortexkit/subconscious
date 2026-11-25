use std::{
    ffi::OsStr,
    io::{self, Write},
};

use super::{
    changed, layer_unavailable,
    layers::{read_layer, LayerLocation},
    RegistryFileId, RegistryRevision,
};
use crate::config_gate::{
    capture::file_object_id,
    root_io::{RootIoJob, RootIoPool},
    GateError, Result,
};
use crate::config_store::{
    ExpectedProjectFile, ProjectFileLayer, ProjectFileTarget, ProjectWriteBackIntent,
};

impl LayerLocation {
    pub(in crate::config_gate) fn expected_file(&self) -> ExpectedProjectFile {
        self.file
            .as_ref()
            .map_or(ExpectedProjectFile::Absent, |file| {
                ExpectedProjectFile::Present {
                    identity: file.id.clone(),
                    digest: file.digest.clone(),
                }
            })
    }
}

impl RegistryRevision {
    pub(in crate::config_gate) fn read_reviewed_workspace(
        &self,
        target: &ProjectFileTarget,
    ) -> Result<ExpectedProjectFile> {
        let reader = self.write_authority(target)?;
        let (_, saved) = self.layer_location(reader, &target.root)?;
        let root = subc_os::open_directory_nofollow_all(&target.root).map_err(|_| changed())?;
        if RegistryFileId::read(&root).map_err(layer_unavailable)? != target.root_identity {
            return Err(changed());
        }
        let leaf = target.relative_path.file_name().ok_or_else(changed)?;
        let current = read_layer(&root, leaf).map_err(target_changed)?;
        let expected = current.expected_file();
        if !saved.same_identity(&current) || saved.expected_file() != expected {
            return Err(changed());
        }
        Ok(expected)
    }

    pub(in crate::config_gate) fn submit_restore(
        &self,
        pool: &RootIoPool,
        restore: crate::config_gate::restore::ProjectRestore,
        archive: std::sync::Arc<crate::config_gate::recovery_inventory::RecoveryArchive>,
        completed: impl FnOnce(&Result<()>) + Send + 'static,
        #[cfg(test)] hook: Option<super::super::runtime::ProjectWriteHook>,
    ) -> Result<RootIoJob<Result<()>>> {
        let revision = self.clone();
        Ok(pool.submit(restore.target.root.clone(), move || {
            let result = revision.restore_proposal(
                restore,
                &archive,
                #[cfg(test)]
                hook,
            );
            completed(&result);
            result
        })?)
    }

    fn restore_proposal(
        &self,
        restore: crate::config_gate::restore::ProjectRestore,
        archive: &crate::config_gate::recovery_inventory::RecoveryArchive,
        #[cfg(test)] hook: Option<super::super::runtime::ProjectWriteHook>,
    ) -> Result<()> {
        let target = &restore.target;
        let reader = self.write_authority(target)?;
        let (root, saved) = self.layer_location(reader, &target.root)?;
        let current_root =
            subc_os::open_directory_nofollow_all(&target.root).map_err(|_| changed())?;
        if RegistryFileId::read(&current_root).map_err(layer_unavailable)? != target.root_identity {
            return Err(changed());
        }
        let leaf = target.relative_path.file_name().ok_or_else(changed)?;
        // Validate the directory entry, then do the whole transaction on the
        // retained handle. A later path substitution cannot redirect a write.
        let current_directory = subc_os::open_directory_nofollow_at(root, OsStr::new(".cortexkit"))
            .map_err(|_| changed())?;
        let directory_id = current_directory.as_ref().map(file_object_id).transpose()?;
        if directory_id != restore.directory_id || directory_id != saved.directory_id {
            return Err(changed());
        }
        let directory = match current_directory {
            Some(directory) => directory,
            None if restore.approved.is_none() => return Ok(()),
            None => subc_os::create_directory_nofollow_at(root, OsStr::new(".cortexkit"))
                .map_err(before_write)?,
        };
        let path = target.path().ok_or_else(changed)?;
        debug_assert_eq!(path.file_name(), Some(leaf));
        crate::config_gate::restore::Restore {
            path: &path,
            directory: &directory,
            expected: crate::config_gate::restore::Expected::Project(&restore.expected),
            approved: restore.approved.as_deref(),
            permissions: restore.permissions,
            archive,
            layer: match target.layer {
                ProjectFileLayer::Workspace { .. } => "workspace",
                ProjectFileLayer::Project { .. } => "project",
            },
        }
        .run(
            || {
                #[cfg(test)]
                if let Some(hook) = hook {
                    hook(false)?;
                }
                Ok(())
            },
            || {},
        )
    }

    pub(in crate::config_gate) fn write_authority(
        &self,
        target: &ProjectFileTarget,
    ) -> Result<&str> {
        let reader = match &target.layer {
            ProjectFileLayer::Project { project_id } => {
                let project = self.project(project_id)?;
                project
                    .project_roots
                    .iter()
                    .any(|root| {
                        root.path == target.root
                            && root.file_id.as_ref() == Some(&target.root_identity)
                    })
                    .then_some(project)
            }
            ProjectFileLayer::Workspace { workspace_id } => self.projects.iter().find(|project| {
                project.workspace_id.as_ref() == Some(workspace_id)
                    && project.workspace_root.as_ref().is_some_and(|root| {
                        root.path == target.root
                            && root.file_id.as_ref() == Some(&target.root_identity)
                    })
                    && self
                        .digest_for_project(&project.project_id)
                        .is_ok_and(|digest| digest == target.registry_revision)
            }),
        }
        .ok_or_else(changed)?;
        if self.module != target.module
            || self.digest_for_project(&reader.project_id)? != target.registry_revision
        {
            return Err(changed());
        }
        Ok(&reader.project_id)
    }

    pub(in crate::config_gate) fn submit_write(
        &self,
        pool: &RootIoPool,
        intent: ProjectWriteBackIntent,
        #[cfg(test)] hook: Option<super::super::runtime::ProjectWriteHook>,
    ) -> Result<RootIoJob<Result<()>>> {
        let revision = self.clone();
        let job = pool.submit(intent.target.root.clone(), move || {
            revision.write_retained(
                &intent,
                #[cfg(test)]
                hook,
            )
        })?;
        Ok(job)
    }

    fn write_retained(
        &self,
        intent: &ProjectWriteBackIntent,
        #[cfg(test)] hook: Option<super::super::runtime::ProjectWriteHook>,
    ) -> Result<()> {
        let target = &intent.target;
        if intent.missing_approved {
            return Err(changed());
        }
        let reader = self.write_authority(target)?;
        let (root, saved) = self.layer_location(reader, &target.root)?;
        let leaf = target.relative_path.file_name().ok_or_else(changed)?;
        let dir_name = OsStr::new(".cortexkit");
        let verify_root = || {
            let current =
                subc_os::open_directory_nofollow_all(&target.root).map_err(|_| changed())?;
            if RegistryFileId::read(&current).map_err(layer_unavailable)? != target.root_identity {
                return Err(changed());
            }
            Ok(())
        };
        verify_root()?;
        let initial = read_layer(root, leaf).map_err(target_changed)?;
        if !saved.same_identity(&initial) || initial.expected_file() != intent.expected {
            return Err(changed());
        }
        let directory = match initial.directory {
            Some(directory) => directory,
            None => subc_os::create_directory_nofollow_at(root, dir_name).map_err(|error| {
                // Publication of the directory alone never changes settings.
                let error = io::Error::from(error);
                if error.kind() == io::ErrorKind::AlreadyExists {
                    changed()
                } else {
                    before_write(error)
                }
            })?,
        };
        let directory_id = file_object_id(&directory).map_err(before_write)?;
        let check_target = || {
            verify_root()?;
            let observed = read_layer(root, leaf).map_err(target_changed)?;
            if observed.directory_id.as_ref() != Some(&directory_id)
                || observed.expected_file() != intent.expected
            {
                return Err(changed());
            }
            Ok(())
        };
        let mut temp = if initial.file.is_some() {
            subc_os::RetainedTempFile::create(&directory)
        } else {
            subc_os::RetainedTempFile::create_for_publish(&directory)
        }
        .map_err(before_write)?;
        temp.file_mut()
            .write_all(&intent.bytes)
            .map_err(before_write)?;
        #[cfg(test)]
        if let Some(hook) = &hook {
            hook(false)?;
        }
        let mut check_error = None;
        let result = if let Some(file) = initial.file {
            temp.replace_checked(leaf, &file.handle, || {
                check_target().map_err(|error| {
                    check_error = Some(error);
                    io::Error::other("configuration changed before replace")
                })
            })
        } else {
            check_target()?;
            temp.publish(leaf)
        };
        result.map_err(|error| {
            if error.renamed() {
                return layer_unavailable("configuration publication could not be confirmed");
            }
            let error = io::Error::from(error);
            check_error.unwrap_or_else(|| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    changed()
                } else {
                    before_write(error)
                }
            })
        })?;
        #[cfg(test)]
        if let Some(hook) = &hook {
            hook(true)?;
        }
        let confirmed = read_layer(root, leaf)
            .map_err(|_| layer_unavailable("configuration publication could not be confirmed"))?;
        if confirmed.directory_id.as_ref() != Some(&directory_id)
            || confirmed.file.is_none_or(|file| file.bytes != intent.bytes)
        {
            return Err(layer_unavailable(
                "configuration publication could not be confirmed",
            ));
        }
        Ok(())
    }
}

fn before_write(error: impl std::fmt::Display) -> GateError {
    GateError::new("write_back_failed", error)
}

fn target_changed(error: GateError) -> GateError {
    if error.code.starts_with("layer_") {
        changed()
    } else {
        error
    }
}
