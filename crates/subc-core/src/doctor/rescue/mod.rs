//! Installation and launch of the local, standalone rescue agent.

mod auth;
mod files;
mod host;
mod pins;
mod store;
mod tools;

#[cfg(test)]
mod tests;

use std::{
    ffi::OsString,
    fmt,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use files::{absent, at, create_session, single_name, Directory, AUTH};
use host::{Children, Host, Io, Platform, System};
use tools::Tools;

const INSTALL_DIR: &str = "cortexkit";
const PROFILE: &str = "pi-profile";
const PROMPT: &str = "rescue-prompt.md";

/// An operational failure that can be printed as one line by the CLI.
#[derive(Debug)]
pub struct RescueError {
    message: String,
}

impl RescueError {
    /// Return the CLI exit code for the failure.
    pub fn exit_code(&self) -> i32 {
        // The CLI rejects usage errors before asking the rescue library to act.
        1
    }
}

impl From<String> for RescueError {
    fn from(message: String) -> Self {
        Self {
            message: message.lines().collect::<Vec<_>>().join(" "),
        }
    }
}

impl fmt::Display for RescueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RescueError {}

/// A signed-in rescue agent with a fresh session and its tool versions held in use.
pub struct PreparedRescue {
    install_root: PathBuf,
    daemon_run_dir: PathBuf,
    session: Directory,
    profile: Directory,
    tools: Tools,
    children: Children,
    provider: &'static str,
}

impl PreparedRescue {
    /// Return the absolute install root used by this run.
    pub fn install_root(&self) -> &Path {
        &self.install_root
    }

    /// Return the resolved local daemon run directory.
    pub fn daemon_run_dir(&self) -> &Path {
        &self.daemon_run_dir
    }

    /// Return the fresh owner-only working directory for this run.
    pub fn session_dir(&self) -> &Path {
        self.session.path()
    }

    /// Create an owner-only session file without replacing an existing entry.
    pub fn write_session_file(&self, name: &str, bytes: &[u8]) -> Result<(), RescueError> {
        if !single_name(name) {
            return Err(RescueError::from(
                "session file name must be a single path component".to_owned(),
            ));
        }
        let path = self.session.path().join(name);
        at(
            &path,
            self.session
                .new_file(name)
                .and_then(|mut file| file.write_all(bytes)),
        )?;
        Ok(())
    }

    /// Start Pi in the session and wait, without treating its exit status as a failure.
    pub fn launch(self) -> Result<(), RescueError> {
        self.launch_with(&System).map_err(Into::into)
    }

    fn launch_with(self, host: &impl Host) -> Result<(), String> {
        at(&self.profile.path().join(AUTH), self.profile.check_auth())?;
        let prompt = self.session.path().join(PROMPT);
        let args = std::iter::once(OsString::from("--system-prompt"))
            .chain(std::iter::once(prompt.into_os_string()))
            .chain(
                [
                    "--no-extensions",
                    "--no-context-files",
                    "--no-skills",
                    "--provider",
                    self.provider,
                ]
                .map(OsString::from),
            );
        let mut child = self
            .tools
            .pi_command(&self.children, self.session.path(), args)?;
        child.io = Io::Terminal;
        host.run(child)
            .map_err(|error| format!("Pi launch: {error}"))?;
        Ok(())
    }
}

/// Resolve the local roots, prepare pinned tools, sign in and create a new session.
pub fn prepare() -> Result<PreparedRescue, RescueError> {
    prepare_with(&System).map_err(Into::into)
}

struct Context<'a, H> {
    host: &'a H,
    profile: &'a Directory,
    tools: &'a Path,
    children: Children,
}

impl<H: Host> Context<'_, H> {
    fn platform(&self) -> Platform {
        self.host.platform()
    }

    fn check_auth(&self) -> Result<(), String> {
        at(&self.profile.path().join(AUTH), self.profile.check_auth())
    }
}

fn managed(directory: &Directory, name: &str) -> Result<Directory, String> {
    at(&directory.path().join(name), directory.managed(name, true))
}

fn absolute_root(path: PathBuf, description: &str) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!(
            "{description} must be absolute: {}",
            path.display()
        ));
    }
    Ok(path)
}

fn prepare_with(host: &impl Host) -> Result<PreparedRescue, String> {
    let data_home = host.data_home()?;
    let daemon_run_dir = absolute_root(host.daemon_run_dir()?, "daemon run directory")?;
    let install_root = data_home.join(INSTALL_DIR);
    let data = Directory::root(&data_home, INSTALL_DIR, true)?
        .ok_or_else(|| format!("{}: missing install root", install_root.display()))?;
    let rescue = managed(&data, "rescue")?;
    let profile = managed(&rescue, PROFILE)?;
    let tools_folder = managed(&data, "tools")?;
    let context = Context {
        host,
        profile: &profile,
        tools: tools_folder.path(),
        children: Children::new(host, profile.path()),
    };
    context.check_auth()?;
    let tools = tools::prepare(&context, &tools_folder)?;
    let provider = auth::sign_in(&context, |args| {
        tools.pi_command(
            &context.children,
            profile.path(),
            args.iter().map(OsString::from),
        )
    })?;
    let sessions = managed(&rescue, "sessions")?;
    let session = create_session(&sessions, &stamp(host.now())?)?;
    let children = context.children;
    Ok(PreparedRescue {
        install_root,
        daemon_run_dir,
        session,
        profile,
        tools,
        children,
        provider,
    })
}

/// The credential path and whether a file was removed there.
pub struct ForgotLogin {
    /// Exact credential path checked, even if it did not exist.
    pub path: PathBuf,
    /// True only when the credential file was deleted.
    pub removed: bool,
}

/// Remove only the local rescue credential, without creating folders or starting children.
pub fn forget() -> Result<ForgotLogin, RescueError> {
    forget_with(&System).map_err(Into::into)
}

fn forget_with(host: &impl Host) -> Result<ForgotLogin, String> {
    let data_home = host.data_home()?;
    let path = data_home
        .join(INSTALL_DIR)
        .join("rescue")
        .join(PROFILE)
        .join(AUTH);
    let mut removed = false;
    if let Some(root) = Directory::root(&data_home, INSTALL_DIR, false)? {
        if let Some(rescue) = at(
            &root.path().join("rescue"),
            absent(root.managed("rescue", false)),
        )? {
            if let Some(profile) = at(
                &rescue.path().join(PROFILE),
                absent(rescue.managed(PROFILE, false)),
            )? {
                removed = at(&path, profile.forget())?;
            }
        }
    }
    Ok(ForgotLogin { path, removed })
}

fn stamp(time: SystemTime) -> Result<String, String> {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("could not name rescue session folder: {error}"))?
        .as_secs();
    // Gregorian calendar conversion in 400-year eras, starting in March so
    // the leap day falls at the end of each year (Howard Hinnant's algorithm).
    let days = seconds / 86_400 + 719_468;
    let era = days / 146_097;
    let day_of_era = days % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let march_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * march_month + 2) / 5 + 1;
    let month = if march_month < 10 {
        march_month + 3
    } else {
        march_month - 9
    };
    let year = era * 400 + year_of_era + u64::from(month <= 2);
    let hour = seconds / 3_600 % 24;
    let minute = seconds / 60 % 60;
    let second = seconds % 60;
    Ok(format!(
        "{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z"
    ))
}
