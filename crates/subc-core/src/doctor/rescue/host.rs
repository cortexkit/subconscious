use std::{
    env,
    ffi::{OsStr, OsString},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    time::{Duration, SystemTime},
};

use crate::setup::{release_index, upgrade_assets};

use super::{
    auth::Login,
    pins::{node_archive, NodeArchive, NODE_DIST},
};

#[derive(Clone, Copy)]
pub(super) struct Platform {
    pub os: &'static str,
    pub arch: &'static str,
}

impl Platform {
    pub fn windows(self) -> bool {
        self.os == "windows"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Io {
    Capture,
    Terminal,
}

#[derive(Clone, Debug)]
pub(super) struct Child {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: Vec<(OsString, OsString)>,
    pub io: Io,
    pub timeout: Option<Duration>,
}

pub(super) struct ChildOutput {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: Vec<u8>,
}

impl ChildOutput {
    fn new(status: ExitStatus, stdout: Vec<u8>) -> Self {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        Self {
            code: status.code(),
            #[cfg(unix)]
            signal: status.signal(),
            #[cfg(windows)]
            signal: None,
            stdout,
        }
    }

    pub fn description(&self) -> String {
        if let Some(code) = self.code {
            format!("exit code {code}")
        } else if let Some(signal) = self.signal {
            format!("terminated by signal {signal}")
        } else {
            "terminated without an exit code".into()
        }
    }
}

pub(super) trait Host {
    fn data_home(&self) -> Result<PathBuf, String>;
    fn daemon_run_dir(&self) -> Result<PathBuf, String>;
    fn platform(&self) -> Platform;
    fn environment(&self) -> Vec<(OsString, OsString)>;
    fn now(&self) -> SystemTime;
    fn run(&self, child: Child) -> Result<ChildOutput, String>;
    fn download(&self, url: &str, destination: &Path) -> Result<(), String>;
    fn extract(&self, archive: &Path, staging: &Path) -> Result<(), String>;
    fn login(&self) -> Result<Option<Login>, String>;

    fn archive(&self) -> Option<NodeArchive> {
        node_archive(self.platform())
    }

    fn node_dist(&self) -> String {
        NODE_DIST.into()
    }

    fn waiting(&self) {
        eprintln!("Waiting for another ck doctor --agent install.");
    }

    fn warn(&self, message: &str) {
        eprintln!("warning: {message}");
    }

    fn remove_version(&self, path: &Path) -> io::Result<()> {
        super::files::remove_entry(path)
    }
}

pub(super) struct System;

impl Host for System {
    fn data_home(&self) -> Result<PathBuf, String> {
        let root = crate::setup::data_directory()?;
        root.parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| format!("rescue install root has no data home: {}", root.display()))
    }

    fn daemon_run_dir(&self) -> Result<PathBuf, String> {
        subc_daemon::daemon_config::daemon_run_dir().map_err(|error| error.to_string())
    }

    fn platform(&self) -> Platform {
        Platform {
            os: env::consts::OS,
            arch: env::consts::ARCH,
        }
    }

    fn environment(&self) -> Vec<(OsString, OsString)> {
        env::vars_os().collect()
    }

    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn run(&self, child: Child) -> Result<ChildOutput, String> {
        let mut command = Command::new(child.program);
        command
            .args(child.args)
            .current_dir(child.cwd)
            .env_clear()
            .envs(child.env);
        if child.io == Io::Terminal {
            let mut process = command
                .spawn()
                .map_err(|error| format!("could not start: {error}"))?;
            let status = process
                .wait()
                .map_err(|error| format!("could not wait: {error}"))?;
            return Ok(ChildOutput::new(status, Vec::new()));
        }
        command
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::piped());
        if let Some(deadline) = child.timeout {
            // The synchronous rescue API may be called from an async CLI. Drive
            // the probe on a separate thread rather than nesting its runtime.
            std::thread::Builder::new()
                .spawn(move || run_probe(command, deadline))
                .map_err(|error| format!("could not start probe thread: {error}"))?
                .join()
                .map_err(|_| "probe thread failed".to_owned())?
        } else {
            let process = command
                .spawn()
                .map_err(|error| format!("could not start: {error}"))?;
            let output = process
                .wait_with_output()
                .map_err(|error| format!("could not wait: {error}"))?;
            Ok(ChildOutput::new(output.status, output.stdout))
        }
    }

    fn download(&self, url: &str, destination: &Path) -> Result<(), String> {
        release_index::download(url, destination)
    }

    fn extract(&self, archive: &Path, staging: &Path) -> Result<(), String> {
        let output = extraction_command(self.platform().windows(), archive, staging)
            .output()
            .map_err(|error| format!("could not extract {}: {error}", archive.display()))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "could not extract {}: {}",
                archive.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    fn login(&self) -> Result<Option<Login>, String> {
        let mut terminal = io::stderr().lock();
        write!(
            terminal,
            "Sign in to Pi for this rescue profile:\n{}\nChoose 1–5, or press Enter to cancel: ",
            Login::menu()
        )
        .and_then(|()| terminal.flush())
        .map_err(|error| format!("could not show Pi login choices: {error}"))?;
        drop(terminal);
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .map_err(|error| format!("could not read Pi login choice: {error}"))?;
        Ok(answer.trim().parse().ok().and_then(Login::choose))
    }

    fn node_dist(&self) -> String {
        #[cfg(feature = "test-support")]
        if let Ok(base) = env::var("CK_NODE_DIST_URL") {
            return base;
        }
        NODE_DIST.into()
    }
}

fn run_probe(command: Command, deadline: Duration) -> Result<ChildOutput, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("could not start probe runtime: {error}"))?;
    runtime.block_on(async {
        let process = tokio::process::Command::from(command)
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("could not start: {error}"))?;
        let output = tokio::time::timeout(deadline, process.wait_with_output())
            .await
            .map_err(|_| format!("timed out after {} ms", deadline.as_millis()))?
            .map_err(|error| format!("could not wait: {error}"))?;
        Ok(ChildOutput::new(output.status, output.stdout))
    })
}

pub(super) fn extraction_command(windows: bool, archive: &Path, staging: &Path) -> Command {
    if windows {
        return upgrade_assets::windows_extract_command(archive, staging);
    }
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            r#"umask 077; exec tar --no-same-owner --no-same-permissions -xzf "$1" -C "$2""#,
            "ck-rescue-untar",
        ])
        .arg(archive)
        .arg(staging);
    command
}

pub(super) struct Children {
    base: Vec<(OsString, OsString)>,
    platform: Platform,
}

impl Children {
    pub fn new(host: &impl Host, profile: &Path) -> Self {
        let platform = host.platform();
        let mut base = host.environment();
        base.retain(|(key, _)| {
            let name = key.to_string_lossy();
            let name = if platform.windows() {
                name.to_ascii_uppercase().into()
            } else {
                name
            };
            !(name.starts_with("SUBC_")
                || name.starts_with("AFT_")
                || name.starts_with("CK_")
                || name == "NODE_OPTIONS"
                || name == "PI_CODING_AGENT_DIR")
        });
        base.push(("PI_CODING_AGENT_DIR".into(), profile.as_os_str().into()));
        base.push(("CK_RESCUE_SESSION".into(), "1".into()));
        Self { base, platform }
    }

    pub fn command(
        &self,
        program: &Path,
        args: Vec<OsString>,
        cwd: &Path,
        private: bool,
    ) -> Result<Child, String> {
        let mut env = self.base.clone();
        if private {
            let folder = program
                .parent()
                .ok_or("private Node executable has no parent folder")?;
            let inherited = env
                .iter()
                .find(|(key, _)| self.matches(key, "PATH"))
                .map(|(_, path)| path);
            let paths = std::iter::once(folder.to_path_buf())
                .chain(inherited.into_iter().flat_map(env::split_paths));
            let path = env::join_paths(paths)
                .map_err(|error| format!("could not prefix private Node to PATH: {error}"))?;
            self.set(&mut env, "PATH", path);
        }
        Ok(Child {
            program: program.into(),
            args,
            cwd: cwd.into(),
            env,
            io: Io::Capture,
            timeout: None,
        })
    }

    pub fn set(&self, env: &mut Vec<(OsString, OsString)>, name: &str, value: OsString) {
        env.retain(|(key, _)| !self.matches(key, name));
        env.push((name.into(), value));
    }

    fn matches(&self, key: &OsStr, name: &str) -> bool {
        if self.platform.windows() {
            key.eq_ignore_ascii_case(name)
        } else {
            key == name
        }
    }
}
