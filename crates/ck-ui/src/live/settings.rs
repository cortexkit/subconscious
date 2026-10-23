//! Read only the file the daemon reports, and only with candidate-preview support.

use std::path::Path;

pub const CAPABILITY: &str = "rescan-candidate-preview/v1";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingsReading {
    pub config_path: Option<String>,
    pub capable: bool,
    pub text: Option<String>,
    pub error: Option<String>,
}

impl SettingsReading {
    pub fn read(config_path: Option<String>, capabilities: &[String]) -> Self {
        let mut reading = Self {
            config_path,
            capable: capabilities.iter().any(|c| c == CAPABILITY),
            ..Self::default()
        };
        if reading.editable() {
            let path = Path::new(reading.config_path.as_deref().unwrap());
            match subc_jsonc::read_document(path) {
                Ok((text, _)) => reading.text = Some(text),
                Err(error) => reading.error = Some(error),
            }
        }
        reading
    }

    pub fn editable(&self) -> bool {
        self.capable
            && self
                .config_path
                .as_deref()
                .is_some_and(|p| Path::new(p).is_absolute())
    }
}
