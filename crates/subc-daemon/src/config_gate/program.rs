//! Resolve once for both program approval and execution. Arguments are never code inputs.

use super::{capture::capture_program, CapturedInput, GateError, InputKey, Result};
use crate::supervise::{ModuleSpec, SpawnProgramBinding};
use std::{collections::BTreeMap, fs::File, io::Read, path::Path};

pub(crate) fn approved_base(snapshot: &super::GateSnapshot, module: &str) -> subc_config::Digest {
    subc_config::raw_digest(
        &serde_json::to_vec(
            &snapshot
                .inputs
                .iter()
                .filter(|(key, _)| key.is_program() && key.module() == Some(module))
                .collect::<Vec<_>>(),
        )
        .expect("program inputs are serializable"),
    )
}

pub(crate) struct ProgramLaunch {
    pub(crate) spec: ModuleSpec,
    pub(crate) inputs: BTreeMap<InputKey, CapturedInput>,
    pub(crate) files: Vec<File>,
    pub(crate) image: subc_os::RetainedFileIdentity,
    bindings: Vec<(std::path::PathBuf, subc_os::RetainedFileIdentity)>,
    #[cfg(windows)]
    _windows_files: Vec<File>,
}

impl ProgramLaunch {
    pub(crate) fn current(&self) -> bool {
        self.bindings.iter().all(|(path, identity)| {
            #[cfg(unix)]
            {
                subc_os::file_identity(path) == Some(*identity)
            }
            #[cfg(windows)]
            {
                std::fs::canonicalize(path)
                    .ok()
                    .and_then(|path| subc_os::open_regular_nofollow(&path).ok())
                    .and_then(|file| subc_os::retained_file_identity(&file).ok())
                    == Some(*identity)
            }
        })
    }
}

pub(crate) fn program_files(entry: &serde_json::Value) -> Result<Vec<String>> {
    let Some(files) = entry.get("program_files") else {
        return Ok(Vec::new());
    };
    files
        .as_array()
        .ok_or_else(|| GateError::new("program_refused", "program_files must be an array"))?
        .iter()
        .map(|file| {
            let path = file
                .as_str()
                .filter(|path| Path::new(path).is_absolute())
                .ok_or_else(|| {
                    GateError::new(
                        "program_refused",
                        "program_files entries must be absolute paths",
                    )
                })?;
            Ok(path.to_owned())
        })
        .collect()
}

fn resolve(spec: &ModuleSpec) -> Result<SpawnProgramBinding> {
    #[cfg(windows)]
    let resolved = {
        let capture = crate::supervise::windows_spawn_capture(spec)
            .map_err(|error| GateError::new("program_refused", error))?;
        let mut resolved = spec.clone();
        resolved.program = capture.path().to_path_buf();
        resolved
    };
    #[cfg(not(windows))]
    let resolved = spec.clone();
    SpawnProgramBinding::capture(&resolved)
        .map_err(|error| GateError::new("program_refused", error))
}

fn shebang(file: &File) -> Result<Option<Vec<String>>> {
    let mut bytes = Vec::new();
    file.try_clone()
        .map_err(|error| GateError::new("program_refused", error))?
        .take(4097)
        .read_to_end(&mut bytes)
        .map_err(|error| GateError::new("program_refused", error))?;
    if !bytes.starts_with(b"#!") {
        return Ok(None);
    }
    let line = bytes
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    if line.len() > 4096 {
        return Err(GateError::new("program_refused", "shebang is too long"));
    }
    let line = std::str::from_utf8(&line[2..])
        .map_err(|_| GateError::new("program_refused", "shebang is not UTF-8"))?;
    let words: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
    if words.is_empty()
        || words.len() > 2
        || (words[0] != "/usr/bin/env" && !Path::new(&words[0]).is_absolute())
    {
        return Err(GateError::new(
            "program_refused",
            "unsupported shebang; use an absolute interpreter and at most one argument",
        ));
    }
    Ok(Some(words))
}

fn known_interpreter(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let name = name.strip_suffix(".exe").unwrap_or(name);
    matches!(name, "node" | "bun" | "deno" | "python" | "sh" | "bash")
        || name.strip_prefix("python").is_some_and(|version| {
            let mut parts = version.split('.');
            let number =
                |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
            parts.next().is_some_and(number)
                && parts.next().is_some_and(number)
                && parts.next().is_none()
        })
}

pub(crate) fn capture(spec: &ModuleSpec, covered: &[String]) -> Result<ProgramLaunch> {
    let binding = resolve(spec)?;
    let mut launch = spec.clone();
    launch.program = std::path::absolute(&binding.path)
        .map_err(|error| GateError::new("program_refused", error))?;
    let mut paths = vec![launch.program.clone()];
    let script = subc_os::open_regular_nofollow(
        &std::fs::canonicalize(&launch.program)
            .map_err(|error| GateError::new("program_refused", error))?,
    )
    .map_err(|error| GateError::new("program_refused", error))?;
    let mut image = binding.identity;
    if let Some(words) = shebang(&script)? {
        let env_form = words[0] == "/usr/bin/env";
        if env_form
            && (words.len() != 2
                || words[1].starts_with('-')
                || words[1].contains(['/', '=', '\\']))
        {
            return Err(GateError::new("program_refused", "env shebang requires exactly one bare program name, without options or assignments"));
        }
        let mut interpreter = spec.clone();
        interpreter.program = if env_form {
            words[1].clone().into()
        } else {
            words[0].clone().into()
        };
        let binding = resolve(&interpreter)?;
        let mut interpreter_file = subc_os::open_regular_nofollow(
            &std::fs::canonicalize(&binding.path)
                .map_err(|error| GateError::new("program_refused", error))?,
        )
        .map_err(|error| GateError::new("program_refused", error))?;
        let mut prefix = [0; 2];
        let count = interpreter_file
            .read(&mut prefix)
            .map_err(|error| GateError::new("program_refused", error))?;
        if count == 2 && prefix == *b"#!" {
            return Err(GateError::new(
                "program_refused",
                "nested shebang interpreters are unsupported",
            ));
        }
        let script_path = launch.program.clone();
        launch.program = std::path::absolute(&binding.path)
            .map_err(|error| GateError::new("program_refused", error))?;
        let mut args = Vec::new();
        if !env_form && words.len() == 2 {
            args.push(words[1].clone());
        }
        args.push(script_path.to_string_lossy().into_owned());
        args.extend(spec.args.clone());
        launch.args = args;
        paths.push(launch.program.clone());
        image = binding.identity;
    }
    let interpreter_only = covered.is_empty() && known_interpreter(&launch.program);
    let launch_identity = image;
    paths.extend(covered.iter().map(Into::into));
    let mut inputs = BTreeMap::new();
    let mut files = Vec::new();
    let mut bindings = Vec::new();
    #[cfg(windows)]
    let mut windows_files = Vec::new();
    for path in paths {
        #[cfg(windows)]
        let no_delete = {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_SHARE_READ excludes both mutation and delete sharing on the
            // same file whose identity is compared with the hash descriptor.
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(1)
                .open(
                    std::fs::canonicalize(&path)
                        .map_err(|error| GateError::new("program_refused", error))?,
                )
                .map_err(|error| GateError::new("program_refused", error))?
        };
        let (file, captured) = capture_program(&path)?;
        let retained = subc_os::retained_file_identity(&file)
            .map_err(|error| GateError::new("program_refused", error))?;
        #[cfg(windows)]
        if retained
            != subc_os::retained_file_identity(&no_delete)
                .map_err(|error| GateError::new("program_refused", error))?
        {
            return Err(GateError::stale());
        }
        if path == launch.program
            && subc_os::retained_file_identity(&file).ok() != Some(launch_identity)
        {
            return Err(GateError::stale());
        }
        inputs.insert(
            InputKey::Program {
                module: spec.module_id.clone(),
                path: path.to_string_lossy().into_owned(),
            },
            captured,
        );
        bindings.push((path, retained));
        #[cfg(windows)]
        windows_files.push(no_delete);
        files.push(file);
    }
    if interpreter_only {
        if let Some(input) = inputs.get_mut(&InputKey::Program {
            module: spec.module_id.clone(),
            path: launch.program.to_string_lossy().into_owned(),
        }) {
            if let subc_config::RecordedValue::Value { value } = &mut input.value {
                value["coverage"] = serde_json::Value::String("interpreter only covered".into());
            }
        }
    }
    Ok(ProgramLaunch {
        spec: launch,
        inputs,
        files,
        image,
        bindings,
        #[cfg(windows)]
        _windows_files: windows_files,
    })
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn config_admission_windows_retains_every_covered_file_without_delete_sharing() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/scratch")
            .join(format!("admission-windows-files-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        let covered = path.join("module.cjs");
        std::fs::write(&covered, "approved code").unwrap();
        let spec = ModuleSpec {
            module_id: "native-binding".into(),
            program: std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                .join("System32/cmd.exe"),
            args: vec!["/C".into(), "exit 0".into()],
            env: vec![],
            reserved: false,
            reserved_prefixes: vec![],
            protocol: subc_control::ModuleProtocol::None,
            overlap: Default::default(),
            carry_containment: false,
        };
        let launch = capture(&spec, &[covered.to_string_lossy().into_owned()]).unwrap();
        assert!(launch.current());
        assert_eq!(
            std::fs::rename(&covered, path.join("replaced.cjs"))
                .unwrap_err()
                .raw_os_error(),
            Some(32)
        );
        drop(launch);
        std::fs::rename(covered, path.join("replaced.cjs")).unwrap();
        std::fs::remove_dir_all(path).unwrap();
    }
}
