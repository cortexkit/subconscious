//! Retained-root and layer observations. All filesystem calls run in a root I/O job.

use std::{ffi::OsStr, fs::File, io::Read, path::Path};

use subc_config::{raw_digest, Digest};
use subc_os::OpenRetainedError;

use super::{layer_unavailable, RegistryFileId, RootCheckFailure};
use crate::config_gate::{
    capture::{file_object_id, FileObjectId, CONFIG_FILE_LIMIT},
    GateError, Result,
};

pub(super) struct OpenedRoot {
    pub id: RegistryFileId,
    pub handle: File,
    pub layer: Option<Result<LayerLocation>>,
}

/// One observation of a physical layer location, even when several tuples read
/// it. A missing parent and an existing directory with no file are distinct.
pub struct LayerLocation {
    pub directory: Option<File>,
    pub directory_id: Option<FileObjectId>,
    pub file: Option<LayerFile>,
}

pub struct LayerFile {
    pub handle: File,
    pub id: FileObjectId,
    pub digest: Digest,
    pub bytes: Vec<u8>,
}

impl LayerLocation {
    pub(in crate::config_gate) fn same_identity(&self, current: &Self) -> bool {
        self.directory_id == current.directory_id
            && self.file.as_ref().map(|file| &file.id) == current.file.as_ref().map(|file| &file.id)
    }
}

pub(super) fn open_root(
    path: &Path,
    layer_leaf: Option<&OsStr>,
    allowed: &[Option<RegistryFileId>],
) -> std::result::Result<OpenedRoot, RootCheckFailure> {
    if !path.is_absolute()
        || path
            .components()
            .collect::<std::path::PathBuf>()
            .as_os_str()
            != path.as_os_str()
    {
        return Err(RootCheckFailure::InvalidPath);
    }
    if path
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(RootCheckFailure::InvalidPath);
    }
    let handle = subc_os::open_directory_nofollow_all(path)
        .map_err(|error| RootCheckFailure::OpenRefused(std::sync::Arc::new(error)))?;
    let id = RegistryFileId::read(&handle)
        .map_err(|error| RootCheckFailure::IdentityUnreadable(std::sync::Arc::new(error)))?;
    // Do not inspect a replacement checkout's settings. Shared roots can
    // have several expectations; only readers with a match will get the file.
    let layer = layer_leaf
        .filter(|_| {
            allowed
                .iter()
                .any(|expected| expected.as_ref().is_none_or(|expected| expected == &id))
        })
        .map(|leaf| read_layer(&handle, leaf));
    Ok(OpenedRoot { id, handle, layer })
}

fn read_layer(root: &File, leaf: &OsStr) -> Result<LayerLocation> {
    let directory =
        subc_os::open_directory_nofollow_at(root, OsStr::new(".cortexkit")).map_err(open_error)?;
    let Some(directory) = directory else {
        return Ok(LayerLocation {
            directory: None,
            directory_id: None,
            file: None,
        });
    };
    let directory_id = file_object_id(&directory).map_err(layer_unavailable)?;
    let file = subc_os::open_single_link_file_nofollow_at(&directory, leaf)
        .map_err(open_error)?
        .map(read_file)
        .transpose()?;
    Ok(LayerLocation {
        directory: Some(directory),
        directory_id: Some(directory_id),
        file,
    })
}

fn read_file(handle: File) -> Result<LayerFile> {
    let id = file_object_id(&handle).map_err(layer_unavailable)?;
    let before = handle.metadata().map_err(layer_unavailable)?;
    let mut bytes = Vec::new();
    (&handle)
        .take(CONFIG_FILE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(layer_unavailable)?;
    if bytes.len() as u64 > CONFIG_FILE_LIMIT {
        return Err(GateError::new(
            "layer_too_large",
            "configuration document exceeds the 1 MiB JSONC limit",
        ));
    }
    let after = handle.metadata().map_err(layer_unavailable)?;
    if before.len() != bytes.len() as u64
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
    {
        return Err(layer_unavailable(
            "configuration layer changed while reading",
        ));
    }
    Ok(LayerFile {
        handle,
        id,
        digest: raw_digest(&bytes),
        bytes,
    })
}

fn open_error(error: OpenRetainedError) -> GateError {
    let code = match error {
        OpenRetainedError::Symlink => "layer_symlink",
        OpenRetainedError::NotRegular => "layer_not_regular",
        OpenRetainedError::Hardlinked => "layer_hardlinked",
        OpenRetainedError::Io(_) => "config_unavailable",
    };
    GateError::new(code, error)
}
