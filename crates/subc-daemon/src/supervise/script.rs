//! Read bundled code once, then transport those exact bytes to the installed runner.

use super::{ScriptEntry, ScriptRuntime};
use crate::config_gate::{CapturedInput, GateError};
use sha2::{Digest, Sha256};
use std::{
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Arc,
};

pub(crate) const LIMIT: u64 = 32 * 1024 * 1024;

pub(crate) fn forbidden_env(key: &str) -> bool {
    let key = if cfg!(windows) {
        key.to_ascii_uppercase()
    } else {
        key.into()
    };
    matches!(
        key.as_str(),
        "NODE_OPTIONS" | "BUN_OPTIONS" | "BUN_INSTALL" | "NODE_PATH"
    ) || key.starts_with("BUN_CONFIG_")
        || key.starts_with("PYTHON")
}

pub(super) fn remove_runtime_env(command: &mut tokio::process::Command, spec: &super::ModuleSpec) {
    let keys = std::env::vars_os()
        .map(|(key, _)| key)
        .chain(spec.env.iter().map(|(key, _)| key.into()))
        .collect::<Vec<_>>();
    for key in keys {
        if forbidden_env(&key.to_string_lossy()) {
            command.env_remove(key);
        }
    }
    for key in ["NODE_OPTIONS", "BUN_OPTIONS", "BUN_INSTALL", "NODE_PATH"] {
        command.env_remove(key);
    }
}

pub(crate) fn status(
    entry: &ScriptEntry,
    approved: Option<&CapturedInput>,
) -> subc_control::ScriptModuleStatus {
    let current = ScriptBytes::read(&entry.path)
        .ok()
        .map(|content| content.input(entry));
    let needs_review = approved.is_some_and(|approved| current.as_ref() != Some(approved));
    let value = approved
        .or(current.as_ref())
        .and_then(|input| match &input.value {
            subc_config::RecordedValue::Value { value } => Some(value),
            _ => None,
        });
    subc_control::ScriptModuleStatus {
        runtime: entry.runtime.as_str().into(),
        // Always null for Python: runner versions await signed release metadata.
        bun_version: None,
        path: entry.path.clone(),
        sha256: value
            .and_then(|value| value.get("raw")?.as_str()?.strip_prefix("sha256:"))
            .map(str::to_owned),
        size: value.and_then(|value| value.get("length")?.as_u64()),
        needs_review,
    }
}

pub(crate) fn runner_beside(daemon: &Path, runtime: ScriptRuntime) -> io::Result<PathBuf> {
    let directory = daemon
        .parent()
        .ok_or_else(|| io::Error::other("daemon install folder is unavailable"))?;
    Ok(directory.join(format!(
        "ck-script-runner-{}{}",
        runtime.as_str(),
        std::env::consts::EXE_SUFFIX
    )))
}

pub(crate) fn runner_path(runtime: ScriptRuntime) -> io::Result<PathBuf> {
    #[cfg(test)]
    {
        Ok(cortexkit_test_support::cargo_profile_bin(match runtime {
            ScriptRuntime::Bun => "script-runner-fixture",
            ScriptRuntime::Python => "script-runner-python-fixture",
        }))
    }
    #[cfg(not(test))]
    {
        runner_beside(&std::env::current_exe()?, runtime)
    }
}

pub(crate) fn resolve_script_path(path: &Path) -> io::Result<PathBuf> {
    let path = if let Ok(suffix) = path.strip_prefix("~") {
        #[cfg(unix)]
        let home = subc_os::real_user_home_dir()?;
        #[cfg(windows)]
        let home = subc_os::profile_dir()?;
        home.join(suffix)
    } else {
        path.into()
    };
    std::path::absolute(path)
}

#[derive(Clone)]
pub(crate) struct ScriptBytes {
    pub(crate) bytes: Arc<[u8]>,
    pub(crate) digest: [u8; 32],
}

impl ScriptBytes {
    pub(crate) fn read(path: &Path) -> Result<Self, GateError> {
        let refused = |error: &dyn std::fmt::Display| {
            GateError::new(
                "program_refused",
                format!("script could not be read: {error}"),
            )
        };
        let file = subc_os::open_regular_nofollow_all(path).map_err(|error| refused(&error))?;
        let mut bytes = Vec::new();
        file.take(LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| refused(&error))?;
        if bytes.len() as u64 > LIMIT {
            return Err(GateError::new(
                "program_refused",
                "script exceeds the 32 MiB limit",
            ));
        }
        let digest = Sha256::digest(&bytes).into();
        Ok(Self {
            bytes: bytes.into(),
            digest,
        })
    }

    pub(crate) fn input(&self, entry: &ScriptEntry) -> CapturedInput {
        let value = serde_json::json!({"kind":"script", "runtime":entry.runtime.as_str(), "path":entry.path, "length":self.bytes.len(), "raw":format!("sha256:{}", self.hex_digest())});
        CapturedInput {
            state: subc_config::InputState::Present {
                digest: subc_config::raw_digest(
                    &serde_json::to_vec(&value).expect("script identity"),
                ),
            },
            value: subc_config::RecordedValue::new(value),
        }
    }

    pub(crate) fn hex_digest(&self) -> String {
        self.digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub(crate) fn header(&self) -> [u8; 52] {
        let mut header = [0; 52];
        header[..8].copy_from_slice(b"CKSCRIPT");
        header[8..12].copy_from_slice(&1u32.to_le_bytes());
        header[12..20].copy_from_slice(&(self.bytes.len() as u64).to_le_bytes());
        header[20..52].copy_from_slice(&self.digest);
        header
    }
}

pub(super) struct Delivery {
    content: ScriptBytes,
    #[cfg(unix)]
    writer: std::fs::File,
    #[cfg(windows)]
    pipe: subc_os::registration_channel::RegistrationPipe,
}

impl Delivery {
    pub(super) fn prepare(
        content: ScriptBytes,
        command: &mut tokio::process::Command,
        #[cfg(unix)] slots: &mut subc_os::registration_channel::ChildDescriptorSlots,
    ) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let (reader, writer, metadata) = subc_os::script_pipe::handoff()?;
            slots.script(reader)?;
            command.env(subc_os::script_pipe::SCRIPT_FD_ENV, metadata);
            Ok(Self { content, writer })
        }
        #[cfg(windows)]
        {
            let pipe = subc_os::registration_channel::RegistrationPipe::new()?;
            command.env(subc_os::script_pipe::SCRIPT_FD_ENV, pipe.name());
            command.env(
                subc_os::registration_channel::DAEMON_PID_ENV,
                std::process::id().to_string(),
            );
            Ok(Self { content, pipe })
        }
    }

    pub(super) fn start(self, _pid: u32) {
        use tokio::io::AsyncWriteExt;
        tokio::spawn(async move {
            let deliver = async {
                #[cfg(unix)]
                let mut writer = tokio::net::unix::pipe::Sender::from_file(self.writer)?;
                #[cfg(windows)]
                let mut writer = tokio::task::spawn_blocking(move || {
                    self.pipe.accept_tokio_pipe_server(
                        _pid,
                        std::time::Instant::now() + std::time::Duration::from_secs(30),
                    )
                })
                .await
                .map_err(io::Error::other)??;
                writer.write_all(&self.content.header()).await?;
                writer.write_all(&self.content.bytes).await?;
                writer.shutdown().await
            };
            // A reader that never consumes its script cannot hold a supervisor
            // task or a transport open indefinitely. Dropping the writer is EOF.
            let _ = tokio::time::timeout(std::time::Duration::from_secs(30), deliver).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_runner_path_is_beside_daemon_not_path_or_config() {
        let installed = std::path::absolute("install/ck-subc").unwrap();
        let expected = installed.parent().unwrap().join(if cfg!(windows) {
            "ck-script-runner-bun.exe"
        } else {
            "ck-script-runner-bun"
        });
        assert_eq!(
            runner_beside(&installed, ScriptRuntime::Bun).unwrap(),
            expected
        );
        let python = installed.parent().unwrap().join(if cfg!(windows) {
            "ck-script-runner-python.exe"
        } else {
            "ck-script-runner-python"
        });
        assert_eq!(
            runner_beside(&installed, ScriptRuntime::Python).unwrap(),
            python
        );
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn script_environment_daemon_inheritance_is_removed_before_runner_start() {
        let root = cortexkit_test_support::ScratchDir::new("script-env");
        for runtime in [ScriptRuntime::Bun, ScriptRuntime::Python] {
            let runner = runner_beside(&root.join("ck-subc"), runtime).unwrap();
            std::fs::copy(
                cortexkit_test_support::cargo_profile_bin("script-runner-fixture"),
                &runner,
            )
            .unwrap();
            let path = root.join("bundle.py");
            let receipt = root.join("receipt.json");
            std::fs::write(&path, b"abc").unwrap();
            let mut parent = std::process::Command::new(&runner);
            parent.args([
                std::ffi::OsStr::new("--env-parent"),
                path.as_os_str(),
                receipt.as_os_str(),
                std::ffi::OsStr::new(runtime.as_str()),
            ]);
            for key in [
                "NODE_OPTIONS",
                "BUN_OPTIONS",
                "BUN_INSTALL",
                "NODE_PATH",
                "BUN_CONFIG_PRELOAD",
                "BUN_CONFIG_any",
                "PYTHONPATH",
                "PYTHONHOME",
            ] {
                parent.env(key, "unapproved");
            }
            let output = parent.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let report: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
            assert_eq!(report["forbidden_env"], serde_json::json!([]));
            assert_eq!(report["runner"], runner.to_string_lossy().as_ref());
            std::fs::remove_file(receipt).unwrap();
        }
    }

    #[test]
    fn script_content_header_has_fixed_independent_bytes() {
        let root = cortexkit_test_support::ScratchDir::new("script-header");
        let path = root.join("bundle.js");
        std::fs::write(&path, b"abc").unwrap();
        let script = ScriptBytes::read(&path).unwrap();
        let hex = script
            .header()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(hex,"434b534352495054010000000300000000000000ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let entry = ScriptEntry::new(ScriptRuntime::Bun, path.clone());
        let approved = script.input(&entry);
        std::fs::write(&path, b"changed").unwrap();
        let status = status(&entry, Some(&approved));
        assert!(status.needs_review);
        assert_eq!(
            status.sha256.as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        assert_eq!(status.size, Some(3));
    }

    #[test]
    fn script_runtime_is_part_of_content_and_launch_identity() {
        let root = cortexkit_test_support::ScratchDir::new("script-runtime-identity");
        let path = root.join("bundle");
        std::fs::write(&path, b"abc").unwrap();
        let content = ScriptBytes::read(&path).unwrap();
        let bun = ScriptEntry::new(ScriptRuntime::Bun, path.clone());
        let python = ScriptEntry::new(ScriptRuntime::Python, path);
        let approved = content.input(&bun);
        assert_ne!(approved, content.input(&python));
        assert!(status(&python, Some(&approved)).needs_review);
        assert_eq!(status(&python, None).runtime, "python");
        let mut spec = crate::daemon_config::parse_with_activity(
            &serde_json::json!({"version":1,"modules":{"example":{"script":{"runtime":"bun","path":bun.path}}}}).to_string(),
            root.join("subc.jsonc"),
        ).unwrap().0.modules[0].module_spec();
        let digest = super::super::LaunchSpecDigest::from_spec(&spec);
        spec.script = Some(python);
        assert_ne!(digest, super::super::LaunchSpecDigest::from_spec(&spec));
    }

    #[test]
    fn script_content_refuses_more_than_32_mib() {
        let root = cortexkit_test_support::ScratchDir::new("script-size");
        let path = root.join("bundle.js");
        // Close the write handle before each read: the read opens the file
        // without write sharing, so Windows refuses it while a writer is open.
        std::fs::File::create(&path)
            .unwrap()
            .set_len(LIMIT)
            .unwrap();
        assert_eq!(ScriptBytes::read(&path).unwrap().bytes.len() as u64, LIMIT);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(LIMIT + 1)
            .unwrap();
        assert_eq!(
            ScriptBytes::read(&path).err().unwrap().code,
            "program_refused"
        );
    }

    #[cfg(unix)]
    #[test]
    fn script_content_refuses_symlinks_in_leaf_and_ancestor() {
        let root = cortexkit_test_support::ScratchDir::new("script-nofollow");
        let directory = root.join("real");
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("bundle.js");
        std::fs::write(&path, b"code").unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&directory, &link).unwrap();
        assert!(ScriptBytes::read(&link.join("bundle.js")).is_err());
        let leaf = root.join("bundle.js");
        std::os::unix::fs::symlink(&path, &leaf).unwrap();
        assert!(ScriptBytes::read(&leaf).is_err());
    }
}
