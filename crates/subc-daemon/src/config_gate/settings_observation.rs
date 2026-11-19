use super::{GateError, Result};
use crate::config_gate::capture::CapturedFile;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use subc_config::InputState;
use subc_control::config::ConfigControlResponse;

pub(crate) struct SettingsObservation {
    pub(crate) response: ConfigControlResponse,
    pub(crate) files: SettingsFiles,
}

#[derive(Default)]
pub(crate) struct SettingsFiles {
    // Keep only the identity and digest from each bounded read, never file bytes.
    observed: BTreeMap<PathBuf, std::result::Result<InputState, (String, String)>>,
    pub(super) write_back_pending: bool,
}

impl SettingsFiles {
    pub(super) fn capture(&mut self, path: &Path) -> Result<CapturedFile> {
        let file = CapturedFile::open(path);
        self.observed.insert(path.to_owned(), fingerprint(&file));
        file
    }

    pub(crate) fn needs_observation(&self) -> bool {
        // Retry outstanding write-backs even when the file looks unchanged.
        // Only a full observation can write, with freshly verified authority.
        self.write_back_pending
            || self.observed.iter().any(|(path, previous)| {
                let current = CapturedFile::open(path);
                current.as_ref().is_err_and(GateError::is_capture_race)
                    || fingerprint(&current) != *previous
            })
    }
}

fn fingerprint(file: &Result<CapturedFile>) -> std::result::Result<InputState, (String, String)> {
    file.as_ref()
        .map(|file| file.state.clone())
        .map_err(|error| (error.code.clone(), error.detail.clone()))
}
