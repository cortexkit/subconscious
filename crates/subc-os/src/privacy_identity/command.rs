use std::{
    ffi::{OsStr, OsString},
    fmt, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

/// A named refusal from the privacy trampoline, using its reserved exit status.
#[derive(Debug)]
pub struct ExecError {
    code: i32,
    detail: String,
}

impl ExecError {
    pub fn exit_code(&self) -> i32 {
        self.code
    }

    #[cfg(target_os = "macos")]
    pub(super) fn new(code: i32, detail: impl fmt::Display) -> Self {
        Self {
            code,
            detail: detail.to_string(),
        }
    }
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {}",
            super::failure_cause(Some(self.code)).unwrap_or("privacy identity failure"),
            self.detail
        )
    }
}

impl std::error::Error for ExecError {}

/// Failure to confirm exec or to validate a trampoline's capability response.
#[derive(Debug)]
pub enum ConfirmationError {
    /// The trampoline refused exec with a named cause.
    Refused(ExecError),
    /// The caller's deadline elapsed without confirmation. Never success.
    TimedOut,
    /// An IO failure or an invalid protocol response.
    Io(io::Error),
}

impl fmt::Display for ConfirmationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(error) => error.fmt(f),
            Self::TimedOut => f.write_str("privacy identity confirmation timed out"),
            Self::Io(error) => write!(f, "privacy identity acknowledgement failed: {error}"),
        }
    }
}

impl std::error::Error for ConfirmationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Refused(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::TimedOut => None,
        }
    }
}

impl From<io::Error> for ConfirmationError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Build a child command with its own macOS responsible-process identity.
///
/// `trampoline` is the caller's binary, whose main must call
/// [`super::trampoline_main`] first. Validate it once with [`probe`] at startup.
/// No plain-exec fallback is ever used on macOS. Elsewhere this is a plain
/// program command and `trampoline` is ignored.
pub struct DisclaimedCommand {
    command: Command,
    #[cfg(target_os = "macos")]
    program: OsString,
    #[cfg(target_os = "macos")]
    args: Vec<OsString>,
}

impl DisclaimedCommand {
    pub fn new(trampoline: impl Into<PathBuf>, program: impl Into<OsString>) -> Self {
        let trampoline = trampoline.into();
        let program = program.into();
        #[cfg(target_os = "macos")]
        {
            Self {
                command: Command::new(trampoline),
                program,
                args: Vec::new(),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = trampoline;
            Self {
                command: Command::new(program),
            }
        }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        #[cfg(target_os = "macos")]
        self.args.push(arg.as_ref().to_owned());
        #[cfg(not(target_os = "macos"))]
        self.command.arg(arg);
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self.arg(arg);
        }
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.command.env(key, value);
        self
    }

    pub fn env_clear(&mut self) -> &mut Self {
        self.command.env_clear();
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.command.env_remove(key);
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.command.current_dir(dir);
        self
    }

    pub fn stdin(&mut self, stdio: impl Into<Stdio>) -> &mut Self {
        self.command.stdin(stdio);
        self
    }

    pub fn stdout(&mut self, stdio: impl Into<Stdio>) -> &mut Self {
        self.command.stdout(stdio);
        self
    }

    pub fn stderr(&mut self, stdio: impl Into<Stdio>) -> &mut Self {
        self.command.stderr(stdio);
        self
    }

    /// Produce a command and its single-use exec confirmation.
    ///
    /// After spawning, **drop the command before confirming**: it owns the
    /// parent's acknowledgement writer. Dropping confirmation before spawn is
    /// safe, but loses the ability to detect a refusal. Do not alter the command's
    /// trampoline arguments. A spawn error is an ordinary IO error; a later
    /// confirmation error requires the caller to kill and reap the child.
    pub fn into_command(self) -> io::Result<(Command, ExecConfirmation)> {
        #[cfg(target_os = "macos")]
        {
            let mut command = self.command;
            let (reader, ack) = super::ExecAcknowledgement::pipe()?;
            command
                .arg("__disclaim-exec")
                .arg(ack.fd().to_string())
                .arg(self.program)
                .args(self.args);
            ack.install_owned(&mut command);
            Ok((command, ExecConfirmation { reader }))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok((self.command, ExecConfirmation {}))
        }
    }

    /// Tokio conversion with the same writer ownership as [`Self::into_command`].
    /// Confirmation is blocking: use a blocking task when inside an async runtime.
    #[cfg(feature = "tokio")]
    pub fn into_tokio_command(self) -> io::Result<(tokio::process::Command, ExecConfirmation)> {
        let (command, confirmation) = self.into_command()?;
        Ok((command.into(), confirmation))
    }
}

/// The existing exec-ack pipe's reader, not a child-exit or readiness check.
///
/// Only a cooperating trampoline's empty EOF confirms exec. The child may have
/// already exited, even with a reserved trampoline status. On non-macOS targets
/// confirmation is a no-op and succeeds immediately, including past deadlines.
#[derive(Debug)]
pub struct ExecConfirmation {
    #[cfg(target_os = "macos")]
    reader: std::io::PipeReader,
}

impl ExecConfirmation {
    /// Block until empty EOF, a tagged refusal, an IO error, or `deadline`.
    /// Drop the originating command before calling; kill/reap on error.
    pub fn confirm(self, deadline: Instant) -> Result<(), ConfirmationError> {
        #[cfg(target_os = "macos")]
        {
            let record = macos::read_until_eof(self.reader, deadline, 1024)?;
            if record.is_empty() {
                return Ok(());
            }
            let cause = std::str::from_utf8(&record)
                .ok()
                .and_then(|record| record.trim().strip_prefix(super::EXEC_REFUSAL_TAG))
                .ok_or_else(|| macos::invalid_response("invalid privacy exec refusal record"))?;
            for code in 120..=123 {
                let name = super::failure_cause(Some(code)).expect("reserved status");
                if let Some(detail) = cause.strip_prefix(name).and_then(|s| s.strip_prefix(": ")) {
                    return Err(ConfirmationError::Refused(ExecError::new(code, detail)));
                }
            }
            Err(macos::invalid_response(
                "unknown privacy exec refusal cause",
            ))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = deadline;
            Ok(())
        }
    }
}

/// Validate the existing `__disclaim-exec --probe` response once at startup.
///
/// On macOS a bad path, failed lookup, wrong response or timeout is an error:
/// callers must refuse child launches, never fall back to inherited grants.
/// The probe uses the parent's environment, not a future child's overrides.
/// Other platforms do not execute the trampoline and immediately return `Ok`.
pub fn probe(trampoline: impl AsRef<Path>, deadline: Instant) -> Result<(), ConfirmationError> {
    #[cfg(target_os = "macos")]
    {
        macos::probe(trampoline.as_ref(), deadline)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (trampoline, deadline);
        Ok(())
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::{io::Write, time::Duration};

    #[test]
    fn malformed_acknowledgements_are_not_exec_confirmation() {
        for record in [
            b"not a tagged refusal\n".as_slice(),
            b"SUBC_PRIVACY_REFUSAL_V1 unknown cause\n".as_slice(),
            &[b'x'; 1025],
        ] {
            let (reader, ack) = super::super::ExecAcknowledgement::pipe().unwrap();
            let mut writer = std::fs::File::from(ack.into_writer());
            writer.write_all(record).unwrap();
            drop(writer);
            let result =
                ExecConfirmation { reader }.confirm(Instant::now() + Duration::from_secs(5));
            assert!(
                matches!(result, Err(ConfirmationError::Io(_))),
                "{result:?}"
            );
        }
    }

    #[test]
    fn partial_refusal_without_eof_still_obeys_deadline() {
        let (reader, ack) = super::super::ExecAcknowledgement::pipe().unwrap();
        let mut writer = std::fs::File::from(ack.into_writer());
        writer.write_all(b"SUBC_PRIVACY_REFUSAL_V1 ").unwrap();
        let result =
            ExecConfirmation { reader }.confirm(Instant::now() + Duration::from_millis(10));
        assert!(
            matches!(result, Err(ConfirmationError::TimedOut)),
            "{result:?}"
        );
        drop(writer);
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::{io::Read, os::fd::AsRawFd, time::Duration};

    pub(super) fn invalid_response(detail: &str) -> ConfirmationError {
        io::Error::new(io::ErrorKind::InvalidData, detail).into()
    }

    /// The reader is nonblocking, so the entire handshake obeys one deadline,
    /// even if a bad trampoline writes a partial record and holds the pipe open.
    pub(super) fn read_until_eof(
        mut reader: io::PipeReader,
        deadline: Instant,
        limit: usize,
    ) -> Result<Vec<u8>, ConfirmationError> {
        let mut record = Vec::new();
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or(ConfirmationError::TimedOut)?;
            // Round up so poll cannot expire before a sub-millisecond deadline.
            let millis =
                remaining.as_millis() + u128::from(remaining.subsec_nanos() % 1_000_000 != 0);
            let mut pollfd = libc::pollfd {
                fd: reader.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one live pipe descriptor and a stack-owned pollfd; poll
            // retains no pointers. The bounded timeout cannot overflow c_int.
            #[allow(unsafe_code)]
            let ready = unsafe { libc::poll(&mut pollfd, 1, millis.min(i32::MAX as u128) as i32) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error.into());
            }
            if ready == 0 {
                continue; // Recheck the absolute deadline, including long waits.
            }
            let mut buffer = [0u8; 256];
            match reader.read(&mut buffer) {
                Ok(0) => return Ok(record),
                Ok(count) if record.len() + count <= limit => {
                    record.extend_from_slice(&buffer[..count]);
                }
                Ok(_) => return Err(invalid_response("privacy acknowledgement is too long")),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    pub(super) fn probe(trampoline: &Path, deadline: Instant) -> Result<(), ConfirmationError> {
        if Instant::now() >= deadline {
            return Err(ConfirmationError::TimedOut);
        }
        // Reuse the pipe's nonblocking reader. The extra writer duplication is
        // harmless here; unlike exec acknowledgement stdout needs no pre-exec hook.
        let (reader, writer) = super::super::ExecAcknowledgement::pipe()?;
        use std::os::fd::OwnedFd;
        let stdout: OwnedFd = writer.into_writer();
        let mut command = Command::new(trampoline);
        command
            .args(["__disclaim-exec", "--probe"])
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::null());
        let mut child = command.spawn()?;
        drop(command);
        let result = (|| {
            let answer = read_until_eof(reader, deadline, 256)?;
            let status = loop {
                if let Some(status) = child.try_wait()? {
                    break status;
                }
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .filter(|remaining| !remaining.is_zero())
                    .ok_or(ConfirmationError::TimedOut)?;
                std::thread::sleep(remaining.min(Duration::from_millis(5)));
            };
            if !status.success() {
                if let Some(code) = status
                    .code()
                    .filter(|code| super::super::failure_cause(Some(*code)).is_some())
                {
                    return Err(ConfirmationError::Refused(ExecError::new(
                        code,
                        "trampoline probe refused",
                    )));
                }
                return Err(invalid_response(
                    "privacy trampoline probe exited unsuccessfully",
                ));
            }
            if std::str::from_utf8(&answer)
                .is_ok_and(|s| s.trim() == super::super::TRAMPOLINE_PROBE)
            {
                Ok(())
            } else {
                Err(invalid_response(
                    "binary does not implement the privacy trampoline protocol",
                ))
            }
        })();
        if result.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        result
    }
}
