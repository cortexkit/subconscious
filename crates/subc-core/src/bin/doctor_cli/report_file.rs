use std::{
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{self, ErrorKind, Write},
    path::{Path, PathBuf},
};

use subc_core::setup;
use subc_daemon::daemon_config;
use subc_transport::connection_file;

use crate::CkError;

const CANDIDATES: u8 = 16;

#[derive(Debug)]
pub(super) struct ReportTarget {
    pub(super) path: PathBuf,
    file: File,
}

impl ReportTarget {
    pub(super) fn create(requested: &Path) -> Result<Self, CkError> {
        let protected = ProtectedPaths::local().map_err(|error| refusal(requested, error))?;
        Self::create_with(requested, &protected)
    }

    fn create_with(requested: &Path, protected: &ProtectedPaths) -> Result<Self, CkError> {
        let name = requested
            .file_name()
            .ok_or_else(|| refusal(requested, "expected a filename"))?;
        #[cfg(windows)]
        if name.to_string_lossy().contains(':') {
            return Err(refusal(
                requested,
                "alternate data streams are not report files",
            ));
        }
        let parent =
            fs::canonicalize(parent_of(requested)).map_err(|error| refusal(requested, error))?;
        let resolved = protected
            .resolve()
            .map_err(|error| refusal(requested, error))?;
        #[cfg(unix)]
        let directory = open_directory(&parent).map_err(|error| refusal(requested, error))?;
        let mut path = parent.join(name);
        for index in 0..CANDIDATES {
            let name = candidate_name(name, index);
            path = parent.join(&name);
            if resolved.contains(&path) {
                return Err(refusal(&path, "the path belongs to the local install, run or settings folder, or is a local connection file"));
            }
            #[cfg(unix)]
            let opened = rustix::fs::openat(
                &directory,
                name.as_os_str(),
                rustix::fs::OFlags::WRONLY
                    | rustix::fs::OFlags::CREATE
                    | rustix::fs::OFlags::EXCL
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            )
            .map(File::from)
            .map_err(io::Error::from);
            #[cfg(windows)]
            let opened = {
                // CREATE_NEW refuses existing names, but Windows can describe an existing
                // directory as access denied rather than as a collision.
                if fs::symlink_metadata(&path).is_ok() {
                    continue;
                }
                subc_os::windows_acl::create_private_file(&path)
            };
            match opened {
                Ok(file) => {
                    #[cfg(windows)]
                    check_created_path(&path, protected)?;
                    return Ok(Self { path, file });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => (),
                Err(error) => return Err(refusal(&path, error)),
            }
        }
        Err(refusal(&path, "all 16 report names already exist"))
    }

    pub(super) fn write(
        &mut self,
        write: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> io::Result<()> {
        if let Err(error) = write(&mut self.file).and_then(|()| self.file.flush()) {
            return match self.file.set_len(0) {
                Ok(()) => Err(error),
                Err(truncate) => Err(io::Error::other(format!(
                    "{error}; also could not truncate the incomplete report: {truncate}"
                ))),
            };
        }
        Ok(())
    }
}

#[derive(Default)]
struct ProtectedPaths {
    folders: Vec<PathBuf>,
    connection_files: Vec<PathBuf>,
}

impl ProtectedPaths {
    fn local() -> Result<Self, String> {
        let config = daemon_config::default_config_path();
        Ok(Self {
            folders: vec![
                setup::data_directory()?,
                daemon_config::daemon_run_dir().map_err(|error| error.to_string())?,
                parent_of(&config).to_path_buf(),
            ],
            connection_files: connection_file::discovery_candidates(None, None),
        })
    }

    fn resolve(&self) -> io::Result<Self> {
        let folders = self
            .folders
            .iter()
            .map(|path| resolve_missing(path))
            .collect::<io::Result<_>>()?;
        let mut connection_files = Vec::new();
        for path in &self.connection_files {
            // Protect the discovery name as well as the target of an existing link.
            let name = path.file_name().ok_or_else(|| {
                io::Error::other(format!("invalid connection file path {path:?}"))
            })?;
            connection_files.push(resolve_missing(parent_of(path))?.join(name));
            connection_files.push(resolve_missing(path)?);
        }
        Ok(Self {
            folders,
            connection_files,
        })
    }

    fn contains(&self, path: &Path) -> bool {
        self.folders
            .iter()
            .any(|folder| path_starts_with(path, folder))
            || self.connection_files.iter().any(|file| {
                path_starts_with(path, file)
                    && path.components().count() == file.components().count()
            })
    }
}

fn resolve_missing(path: &Path) -> io::Result<PathBuf> {
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        let current = if ancestor.as_os_str().is_empty() {
            Path::new(".")
        } else {
            ancestor
        };
        match fs::canonicalize(current) {
            Ok(mut resolved) => {
                resolved.extend(missing.into_iter().rev());
                return Ok(resolved);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                match fs::symlink_metadata(current) {
                    // A dangling link may point at a report name we must not create.
                    Ok(_) => {
                        return Err(io::Error::other(format!(
                            "cannot resolve protected path {current:?}"
                        )))
                    }
                    Err(error) if error.kind() == ErrorKind::NotFound => (),
                    Err(error) => return Err(error),
                }
                let (Some(parent), Some(name)) = (ancestor.parent(), ancestor.file_name()) else {
                    return Err(error);
                };
                missing.push(name);
                ancestor = parent;
            }
            Err(error) => {
                return Err(io::Error::other(format!(
                    "cannot resolve protected path {current:?}: {error}"
                )))
            }
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn path_starts_with(path: &Path, prefix: &Path) -> bool {
    path.starts_with(prefix)
}

#[cfg(any(windows, target_os = "macos"))]
fn path_starts_with(path: &Path, prefix: &Path) -> bool {
    // A protected name that doesn't exist yet has no on-disk casing to canonicalize.
    let mut components = path.components();
    prefix.components().all(|part| {
        components.next().is_some_and(|actual| {
            actual.as_os_str().to_string_lossy().to_uppercase()
                == part.as_os_str().to_string_lossy().to_uppercase()
        })
    })
}

#[cfg(unix)]
fn open_directory(path: &Path) -> io::Result<rustix::fd::OwnedFd> {
    use rustix::fs::{open, openat, Mode, OFlags};

    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut directory = open("/", flags, Mode::empty())?;
    // The parent was canonicalized; any symlink that appears in it now is a replacement.
    for name in path.iter().skip(1) {
        directory = openat(directory, name, flags, Mode::empty())?;
    }
    Ok(directory)
}

#[cfg(windows)]
fn check_created_path(path: &Path, protected: &ProtectedPaths) -> Result<(), CkError> {
    let refused = |at: &Path, reason: &dyn std::fmt::Display| {
        refusal(
            at,
            format_args!("{reason}; the new empty private file was left in place"),
        )
    };
    let actual = fs::canonicalize(path).map_err(|error| refused(path, &error))?;
    if actual != path
        || protected
            .resolve()
            .map_err(|error| refused(&actual, &error))?
            .contains(&actual)
    {
        return Err(refused(
            &actual,
            &"the report location changed during creation",
        ));
    }
    Ok(())
}

fn refusal(path: &Path, reason: impl std::fmt::Display) -> CkError {
    CkError::Usage(format!("doctor report {path:?}: {reason}"))
}

fn parent_of(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn candidate_name(name: &OsStr, index: u8) -> OsString {
    if index == 0 {
        return name.to_os_string();
    }
    let path = Path::new(name);
    let mut suffixed = path.file_stem().unwrap_or(name).to_os_string();
    suffixed.push(format!("-{index}"));
    if let Some(extension) = path.extension() {
        suffixed.push(".");
        suffixed.push(extension);
    }
    suffixed
}

#[cfg(test)]
mod tests {
    use cortexkit_test_support::ScratchDir;
    use serde_json::Value;

    use super::*;
    use crate::doctor_cli::{complete, tests::diagnostic, write_full_report, Options};
    use crate::{Command, JsonOutput};

    fn assert_refused(path: &Path, protected: &ProtectedPaths) {
        let error = ReportTarget::create_with(path, protected).unwrap_err();
        assert!(error.to_string().contains("local install"), "{error}");
        assert_eq!(error.exit_code(), 2);
        assert_eq!(
            fs::symlink_metadata(path).unwrap_err().kind(),
            ErrorKind::NotFound
        );
    }

    #[test]
    fn refuses_a_parent_inside_the_install_root() {
        let dir = ScratchDir::new("doctor-report-install-root");
        let root = dir.join("install");
        let inside = root.join("inside");
        fs::create_dir_all(&inside).unwrap();
        let protected = ProtectedPaths {
            folders: vec![root.clone()],
            ..ProtectedPaths::default()
        };
        assert_refused(&inside.join("report.json"), &protected);
        assert!(!root.join("stack-stopped").exists());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_an_aliased_parent_inside_the_run_folder() {
        let dir = ScratchDir::new("doctor-report-run-alias");
        let run = dir.join("run");
        let alias = dir.join("alias");
        fs::create_dir(&run).unwrap();
        std::os::unix::fs::symlink(&run, &alias).unwrap();
        let protected = ProtectedPaths {
            folders: vec![run.clone()],
            ..ProtectedPaths::default()
        };
        assert_refused(&alias.join("report.json"), &protected);
        assert!(!run.join("report.json").exists());
    }

    #[test]
    fn refuses_absent_protected_names_including_collision_suffixes() {
        let dir = ScratchDir::new("doctor-report-absent-targets");
        let root = dir.join("install");
        let connection = dir.join("subc-connection.json");
        let suffix = dir.join("report-1.json");
        let protected = ProtectedPaths {
            folders: vec![root.clone()],
            connection_files: vec![connection.clone(), suffix.clone()],
        };
        assert_refused(&root, &protected);
        assert_refused(&connection, &protected);
        let first = dir.join("report.json");
        fs::write(&first, "keep").unwrap();
        assert!(ReportTarget::create_with(&first, &protected)
            .unwrap_err()
            .to_string()
            .contains("local connection file"));
        assert!(!suffix.exists());
        assert_eq!(fs::read_to_string(&first).unwrap(), "keep");
    }

    #[test]
    fn tries_only_sixteen_names() {
        let dir = ScratchDir::new("doctor-report-collisions");
        let first = dir.join("diagnosis.json");
        fs::write(&first, "keep").unwrap();
        for index in 1..16 {
            fs::write(dir.join(format!("diagnosis-{index}.json")), "keep").unwrap();
        }
        let error = ReportTarget::create_with(&first, &ProtectedPaths::default()).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("diagnosis-15.json"));
        assert!(!dir.join("diagnosis-16.json").exists());
        assert_eq!(fs::read_to_string(first).unwrap(), "keep");
    }

    #[test]
    fn saves_the_same_document_with_tails_at_the_numbered_path() {
        let dir = ScratchDir::new("doctor-report-saved");
        let first = dir.join("diagnosis.json");
        let unrelated = dir.join("unrelated");
        fs::write(&unrelated, "keep").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&unrelated, &first).unwrap();
        #[cfg(windows)]
        fs::write(&first, "keep").unwrap();
        let target = ReportTarget::create_with(&first, &ProtectedPaths::default()).unwrap();
        let path = target.path.clone();
        assert_eq!(path.file_name().unwrap(), "diagnosis-1.json");
        let output =
            JsonOutput::for_command(&Command::Doctor(Options::default()), true, false).unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = complete(
            diagnostic(0),
            Some(target),
            output,
            &mut stdout,
            &mut stderr,
            write_full_report,
        )
        .unwrap();
        assert_eq!(exit, 0);
        assert!(stderr.is_empty());
        let document: Value = serde_json::from_slice(&stdout).unwrap();
        let mut full: Value = serde_json::from_reader(File::open(&path).unwrap()).unwrap();
        assert_eq!(
            full.as_object_mut().unwrap().remove("log_tails"),
            Some(serde_json::json!([]))
        );
        assert_eq!(full, document);
        assert_eq!(
            document["report"],
            serde_json::json!({"path": path, "sanitized": false})
        );
        assert_eq!(fs::read_to_string(&unrelated).unwrap(), "keep");
        assert_eq!(fs::read_to_string(&first).unwrap(), "keep");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert!(fs::symlink_metadata(&first).unwrap().is_symlink());
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        }
    }

    #[test]
    fn partial_write_is_truncated_and_json_names_no_report() {
        let dir = ScratchDir::new("doctor-report-write-error");
        let name = dir.join("diagnosis.json");
        let target = ReportTarget::create_with(&name, &ProtectedPaths::default()).unwrap();
        let output =
            JsonOutput::for_command(&Command::Doctor(Options::default()), true, false).unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = complete(
            diagnostic(0),
            Some(target),
            output,
            &mut stdout,
            &mut stderr,
            |file, _| {
                file.write_all(b"partial document")?;
                assert_eq!(file.metadata()?.len(), 16);
                Err(io::Error::other("simulated write failure"))
            },
        )
        .unwrap();
        assert_eq!(exit, 1);
        assert_eq!(fs::metadata(&name).unwrap().len(), 0);
        let document: Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(document["ok"], true);
        assert_eq!(document["report"], Value::Null);
        let error = String::from_utf8(stderr).unwrap();
        assert_eq!(error.lines().count(), 1);
        assert!(error.contains("diagnosis.json"));
        assert!(error.contains("simulated write failure"));
    }
}
