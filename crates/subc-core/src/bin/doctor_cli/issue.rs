use std::io::{self, Write};

use subc_core::doctor::{CheckedProgram, Interrupt, LogFallback};

use super::Terminal;

#[derive(Default)]
pub(super) struct Rows(Vec<Row>);

struct Row {
    module: String,
    outcome: String,
    path: Option<std::path::PathBuf>,
    notice: Option<&'static str>,
    failed: bool,
}

impl Rows {
    pub(super) fn failed(&self) -> bool {
        self.0.iter().any(|row| row.failed)
    }

    pub(super) fn write(&self, out: &mut impl Write) -> io::Result<()> {
        if self.0.is_empty() {
            return writeln!(out, "No module findings to file.");
        }
        for row in &self.0 {
            write!(out, "{}: {}", row.module, row.outcome)?;
            if let Some(path) = &row.path {
                write!(out, " [{}]", path.display())?;
            }
            writeln!(out)?;
            if let Some(notice) = row.notice {
                writeln!(out, "  {notice}")?;
            }
        }
        Ok(())
    }
}

pub(super) use filing::file;

mod filing {
    use std::{
        ffi::{OsStr, OsString},
        fs::File,
        path::{Path, PathBuf},
        process::{Output, Stdio},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use cortexkit_doctor::{
        build_local_report, build_report, redact_secrets, runner, Manifest, ModuleId,
        ReportContext, ReportInput,
    };
    #[cfg(unix)]
    use std::os::fd::OwnedFd;

    #[cfg(unix)]
    use rustix::{
        fs::{fchmod, fstat, mkdirat, open, openat, unlinkat, AtFlags, Mode, OFlags},
        io::Errno,
        process::geteuid,
    };
    use subc_core::{
        doctor::{run_program_pool, Prompter},
        setup,
    };

    use super::super::super::utc_parts_from_epoch_secs;
    use super::*;

    const PUBLIC_CHECK_LIMIT: Duration = Duration::from_secs(10);
    const GH_LIMIT: Duration = Duration::from_secs(60);
    const REPORT_BYTES: usize = 64 * 1024;
    const FALLBACK_URL_CHARS: usize = 8_000;
    #[cfg(unix)]
    const DIRECTORY_FLAGS: OFlags = OFlags::DIRECTORY
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);
    #[cfg(unix)]
    const DIRECTORY_MODE: Mode = Mode::from_bits_truncate(0o700);
    #[cfg(unix)]
    const FILE_MODE: Mode = Mode::from_bits_truncate(0o600);

    enum ManifestOutcome {
        Valid(Manifest),
        Invalid,
        Skipped(&'static str),
    }

    pub(in super::super) fn file(
        mut checked: Vec<CheckedProgram>,
        terminal: &Terminal,
        interrupt: &Interrupt,
        logs: &dyn LogFallback,
        github_api: Option<OsString>,
    ) -> Rows {
        checked.retain(|program| !program.check.findings().is_empty());
        checked.sort_unstable_by(|a, b| a.module_id.cmp(&b.module_id));
        let limits = runner::Limits::default();
        let options = runner::Options::new();
        let manifests = run_program_pool(
            checked.iter().collect(),
            |program| (program, manifest(program, interrupt, &options, limits)),
            |program| (program, ManifestOutcome::Skipped("manifest_not_started")),
        );
        let flow = Flow {
            terminal,
            interrupt,
            logs,
            github_api,
        };
        Rows(
            manifests
                .into_iter()
                .map(|(program, manifest)| flow.handle(program, manifest))
                .collect(),
        )
    }

    fn manifest(
        program: &CheckedProgram,
        interrupt: &Interrupt,
        options: &runner::Options,
        limits: runner::Limits,
    ) -> ManifestOutcome {
        if !program.unchanged() {
            return ManifestOutcome::Skipped("program_changed");
        }
        let deadline =
            Instant::now() + limits.manifest + limits.term_to_kill.max(limits.exit_grace);
        let Some(_guard) = interrupt.protect("a module manifest", deadline) else {
            return ManifestOutcome::Skipped("manifest_not_started");
        };
        match runner::manifest(&program.path, options) {
            Ok(manifest) => ManifestOutcome::Valid(manifest),
            // A rejected repository (DecodeError::Repository) is an invalid
            // manifest too. The local report contains the check, not the manifest error.
            Err(runner::RunError::InvalidOutput(_)) => ManifestOutcome::Invalid,
            Err(runner::RunError::TimedOut) => ManifestOutcome::Skipped("manifest_timed_out"),
            Err(runner::RunError::Crashed(_)) => ManifestOutcome::Skipped("manifest_crashed"),
            Err(runner::RunError::NoDoctor) => ManifestOutcome::Skipped("manifest_no_doctor"),
            Err(_) => ManifestOutcome::Skipped("manifest_spawn_failed"),
        }
    }

    fn save_local(program: &CheckedProgram, module_id: &ModuleId) -> io::Result<PathBuf> {
        let data = setup::data_directory().map_err(io::Error::other)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?;
        let ctx = report_context(&data, now)?;
        let body = build_local_report(&ctx, module_id, &program.check).map_err(io::Error::other)?;
        save(&data, now.as_secs(), "json", &body)
    }

    struct Flow<'a> {
        terminal: &'a Terminal,
        interrupt: &'a Interrupt,
        logs: &'a dyn LogFallback,
        github_api: Option<OsString>,
    }

    impl Flow<'_> {
        fn handle(&self, program: &CheckedProgram, manifest: ManifestOutcome) -> Row {
            let mut row = Row {
                module: program.module_id.clone(),
                outcome: "skipped interrupted".into(),
                path: None,
                notice: None,
                failed: false,
            };
            if self.interrupt.signal().is_some() {
                return row;
            }
            let manifest = match manifest {
                ManifestOutcome::Valid(manifest) => manifest,
                ManifestOutcome::Invalid => {
                    // Configured module IDs can contain characters such as colons
                    // that the report format does not allow. Do not substitute an ID.
                    let Ok(module_id) = program.module_id.parse() else {
                        row.save_failed();
                        row.notice = Some("The module ID is not valid for the report format.");
                        return row;
                    };
                    row.saved(save_local(program, &module_id), "skipped manifest_invalid");
                    if row.path.is_some() {
                        row.notice =
                            Some("The module's manifest was invalid. This report is local only.");
                    }
                    return row;
                }
                ManifestOutcome::Skipped(reason) => {
                    row.outcome = format!("skipped {reason}");
                    return row;
                }
            };
            let repository = github_repository(manifest.repository());
            let default_title = || {
                format!(
                    "{}: {}",
                    program.module_id,
                    program.check.findings()[0].id()
                )
            };
            let can_prompt = repository.is_some() && self.terminal.interactive();
            let (title, description) = if can_prompt {
                let prompt = format!("\n{}: Issue title (required): ", program.module_id);
                let title = loop {
                    let Some(title) = self.terminal.ask(&prompt) else {
                        row.outcome = "skipped title_aborted".into();
                        return row;
                    };
                    if !title.is_empty() {
                        break title;
                    }
                };
                let description = self
                    .terminal
                    .ask("Issue description (optional): ")
                    .unwrap_or_default();
                (title, description)
            } else {
                (default_title(), String::new())
            };

            if self.interrupt.signal().is_some() {
                return row;
            }
            let title = redact_secrets(&title);
            match build_and_save(program, &manifest, self.logs, &title, &description) {
                Ok((body, path)) => {
                    if let Some(repository) = repository {
                        row.path = Some(path);
                        self.handle_saved(repository, &title, &body, &mut row);
                    } else {
                        row.saved(Ok(path), "skipped repository_invalid");
                    }
                }
                Err(error) => row.saved(Err(error), ""),
            }
            row
        }

        fn handle_saved(&self, repository: &str, title: &str, body: &str, row: &mut Row) {
            if !self.terminal.interactive() {
                row.outcome = "saved_only".into();
                #[cfg(windows)]
                {
                    row.notice =
                        Some("The answer must be typed in a console window to file the issue.");
                }
                return;
            }
            let Some(guard) = self.interrupt.protect(
                "the public repository check",
                Instant::now() + PUBLIC_CHECK_LIMIT,
            ) else {
                return;
            };
            let public = is_public(repository, self.github_api.as_deref());
            drop(guard);
            if !public {
                row.outcome = "not_public".into();
                return;
            }
            if self.interrupt.signal().is_some() {
                return;
            }
            let prompt = format!("\nTitle: {title}\n{body}\nFile this report as a public issue in github.com/{repository}? [y/N] ");
            if !self.terminal.yes(&prompt) {
                row.outcome = "declined".into();
                return;
            }
            let Some(auth) = run_gh(
                self.interrupt,
                "gh auth status",
                gh().args(["auth", "status", "--hostname", "github.com"]),
            ) else {
                return;
            };
            let path = row.path.as_ref().expect("saved report path");
            if auth.is_ok_and(|result| result.status.success()) {
                let Some(created) = run_gh(
                    self.interrupt,
                    "gh issue create",
                    gh().args([
                        "issue",
                        "create",
                        "--repo",
                        &format!("github.com/{repository}"),
                        "--title",
                        title,
                        "--body-file",
                    ])
                    .arg(path),
                ) else {
                    return;
                };
                if let Ok(result) = created {
                    let url = String::from_utf8_lossy(&result.stdout);
                    let url = url.trim();
                    if result.status.success()
                        && url.starts_with("https://github.com/")
                        && !url.chars().any(char::is_whitespace)
                    {
                        row.outcome = format!("filed {url}");
                        return;
                    }
                }
                row.failed = true;
            }
            let url = fallback_url(repository, title, body);
            let _ = self
                .terminal
                .write(&format!("\n{url}\nSaved report: {}\n", path.display()));
            row.outcome = "url_printed".into();
        }
    }

    impl Row {
        fn saved(&mut self, result: io::Result<PathBuf>, outcome: &str) {
            match result {
                Ok(path) => {
                    self.outcome = outcome.into();
                    self.path = Some(path);
                }
                Err(_) => self.save_failed(),
            }
        }

        fn save_failed(&mut self) {
            self.outcome = "skipped save_failed".into();
            self.failed = true;
        }
    }

    fn github_repository(url: &str) -> Option<&str> {
        fn part(segment: &str) -> bool {
            !matches!(segment, "" | "." | "..")
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        }
        let slug = url.strip_prefix("https://github.com/")?;
        let slug = slug.strip_suffix('/').unwrap_or(slug);
        let slug = slug.strip_suffix(".git").unwrap_or(slug);
        let (owner, repo) = slug.split_once('/')?;
        (part(owner) && part(repo)).then_some(slug)
    }

    fn is_public(repository: &str, test_api: Option<&OsStr>) -> bool {
        #[derive(serde::Deserialize)]
        struct Repository {
            private: bool,
        }

        let base = test_api
            .and_then(OsStr::to_str)
            .unwrap_or("https://api.github.com");
        let url = format!("{}/repos/{repository}", base.trim_end_matches('/'));
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .max_redirects(0)
                .proxy(None)
                .timeout_global(Some(PUBLIC_CHECK_LIMIT))
                .build(),
        );
        let Ok(mut response) = agent
            .get(&url)
            .header("User-Agent", concat!("ck/", env!("CARGO_PKG_VERSION")))
            .call()
        else {
            return false;
        };
        response.status() == 200
            && response
                .body_mut()
                .read_to_vec()
                .ok()
                .and_then(|body| serde_json::from_slice::<Repository>(&body).ok())
                .is_some_and(|repo| !repo.private)
    }

    fn gh() -> tokio::process::Command {
        let mut command = tokio::process::Command::new("gh");
        command
            .env("GH_HOST", "github.com")
            .env("GH_PROMPT_DISABLED", "1")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        command
    }

    fn run_gh(
        interrupt: &Interrupt,
        operation: &'static str,
        command: &mut tokio::process::Command,
    ) -> Option<io::Result<Output>> {
        let deadline = Instant::now() + GH_LIMIT;
        let _guard = interrupt.protect(operation, deadline)?;
        Some(tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                tokio::time::timeout_at(deadline.into(), gh_output(command)).await?
            })
        }))
    }

    #[cfg(unix)]
    async fn gh_output(command: &mut tokio::process::Command) -> io::Result<Output> {
        command.output().await
    }

    #[cfg(windows)]
    async fn gh_output(command: &mut tokio::process::Command) -> io::Result<Output> {
        use subc_jobobject::{ContainedChild, JobObject, CREATE_SUSPENDED_FLAG};

        // A separate process group keeps console Ctrl-C from interrupting gh.
        // Assign while suspended so even children holding the output pipes are
        // killed when the timeout drops the job.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        let job = JobObject::new()?;
        let child = command
            .creation_flags(CREATE_SUSPENDED_FLAG | CREATE_NEW_PROCESS_GROUP)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let pid = child.id().expect("a newly spawned child has a pid");
        let ContainedChild { child, job: _job } = ContainedChild::contain(child, job, pid)?;
        child.wait_with_output().await
    }

    fn fallback_url(repo: &str, title: &str, body: &str) -> String {
        let prefix = format!(
            "https://github.com/{repo}/issues/new?title={}&body=",
            quote(title)
        );
        let encoded = quote(body);
        let body = if prefix.len() + encoded.len() > FALLBACK_URL_CHARS {
            quote("Please paste the contents of the saved report file here.")
        } else {
            encoded
        };
        prefix + &body
    }

    fn quote(value: &str) -> String {
        use std::fmt::Write as _;

        let mut encoded = String::new();
        for byte in value.bytes() {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                encoded.push(char::from(byte));
            } else {
                write!(encoded, "%{byte:02X}").expect("formatting into a string");
            }
        }
        encoded
    }

    fn report_context(data: &Path, now: Duration) -> io::Result<ReportContext> {
        let config = subc_daemon::daemon_config::default_config_path();
        let mut key = [0; 32];
        getrandom::fill(&mut key).map_err(|error| io::Error::other(error.to_string()))?;
        Ok(ReportContext::new(
            key,
            now.as_millis() as i64,
            data,
            config.parent().unwrap_or(&config),
            std::env::temp_dir(),
        ))
    }

    fn build_and_save(
        program: &CheckedProgram,
        manifest: &Manifest,
        logs: &dyn LogFallback,
        title: &str,
        description: &str,
    ) -> io::Result<(String, PathBuf)> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?;
        let data = setup::data_directory().map_err(io::Error::other)?;
        let ctx = report_context(&data, now)?;
        let log_tail = logs.tail(&program.module_id).tail.ok().flatten();
        let lines = log_tail
            .as_ref()
            .into_iter()
            .flat_map(|tail| tail.entries.iter().map(String::as_str))
            .collect::<Vec<_>>();
        let input = ReportInput::new(
            manifest.module_id(),
            manifest,
            title,
            description,
            Ok(&program.check),
            REPORT_BYTES,
        )
        .with_log_tails(&lines);
        let body = String::from_utf8(build_report(&ctx, &input).map_err(io::Error::other)?)
            .map_err(io::Error::other)?;
        let path = save(&data, now.as_secs(), "md", body.as_bytes())?;
        Ok((body, path))
    }

    fn save(data: &Path, seconds: u64, extension: &str, body: &[u8]) -> io::Result<PathBuf> {
        let (reports, directory) = report_directory(data)?;
        let utc = utc_parts_from_epoch_secs(seconds);
        let (year, month, day, hour, minute, second) = (
            utc.year,
            utc.month,
            utc.day,
            utc.hour,
            utc.minute,
            seconds % 60,
        );
        for _ in 0..16 {
            let mut suffix = [0; 8];
            getrandom::fill(&mut suffix).map_err(|error| io::Error::other(error.to_string()))?;
            let suffix = u64::from_be_bytes(suffix);
            let name = format!(
                "{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z-{suffix:016x}.{extension}"
            );
            let path = directory.join(&name);
            match write_new(&reports, &path, body) {
                Ok(()) => return Ok(path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
                Err(error) => return Err(error),
            }
        }
        Err(io::ErrorKind::AlreadyExists.into())
    }

    #[cfg(unix)]
    fn report_directory(data: &Path) -> io::Result<(OwnedFd, PathBuf)> {
        let data_fd = match open(data, DIRECTORY_FLAGS | OFlags::RDONLY, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => {
                let (Some(parent), Some(name)) = (data.parent(), data.file_name()) else {
                    return Err(Errno::NOENT.into());
                };
                let parent = open(parent, DIRECTORY_FLAGS | OFlags::RDONLY, Mode::empty())?;
                private_dir(&parent, name)?
            }
            Err(error) => return Err(error.into()),
        };
        let doctor = private_dir(&data_fd, OsStr::new("doctor"))?;
        let reports = private_dir(&doctor, OsStr::new("reports"))?;
        Ok((reports, data.join("doctor/reports")))
    }

    #[cfg(unix)]
    fn write_new(reports: &OwnedFd, path: &Path, body: &[u8]) -> io::Result<()> {
        let name = path.file_name().expect("generated report filename");
        let fd = openat(
            reports,
            name,
            OFlags::WRONLY | OFlags::CLOEXEC | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
            FILE_MODE,
        )?;
        let result = (|| {
            if Mode::from_raw_mode(fstat(&fd)?.st_mode) != FILE_MODE {
                fchmod(&fd, FILE_MODE)?;
            }
            File::from(fd).write_all(body)
        })();
        if result.is_err() {
            let _ = unlinkat(reports, name, AtFlags::empty());
        }
        result
    }

    #[cfg(windows)]
    fn report_directory(data: &Path) -> io::Result<(Vec<File>, PathBuf)> {
        use subc_os::windows_acl::verify_private_dir;

        let parent = data.parent().ok_or(io::ErrorKind::InvalidInput)?;
        let mut names = parent
            .ancestors()
            .take_while(|path| path.is_absolute())
            .collect::<Vec<_>>();
        names.reverse();
        let mut handles = names
            .into_iter()
            .map(open_dir)
            .collect::<io::Result<Vec<_>>>()?;
        let doctor = data.join("doctor");
        let reports = doctor.join("reports");
        for path in [data, doctor.as_path(), reports.as_path()] {
            handles.push(match open_dir(path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    match subc_os::create_private_directory_exclusive(path) {
                        Ok(()) => (),
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
                        Err(error) => return Err(error),
                    }
                    open_dir(path)?
                }
                result => result?,
            });
            if path != data {
                // Refuse an existing directory with broad grants instead of
                // rewriting its ACL. The pinned handles rule out path redirects.
                verify_private_dir(path)?;
            }
        }
        Ok((handles, reports))
    }

    #[cfg(windows)]
    fn open_dir(path: &Path) -> io::Result<File> {
        subc_os::open_directory_nofollow_all(path).map_err(|error| match error {
            subc_os::OpenRegularError::Io(error) => error,
            other => io::Error::new(io::ErrorKind::InvalidInput, other),
        })
    }

    #[cfg(windows)]
    fn write_new(_reports: &[File], path: &Path, body: &[u8]) -> io::Result<()> {
        let mut file = subc_os::windows_acl::create_private_file(path)?;
        let result = file.write_all(body);
        if result.is_err() {
            drop(file);
            let _ = std::fs::remove_file(path);
        }
        result
    }

    #[cfg(unix)]
    fn private_dir(parent: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
        let created = match mkdirat(parent, name, DIRECTORY_MODE) {
            Ok(()) => true,
            Err(Errno::EXIST) => false,
            Err(error) => return Err(error.into()),
        };
        let fd = match openat(
            parent,
            name,
            DIRECTORY_FLAGS | OFlags::RDONLY,
            Mode::empty(),
        ) {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Err(Errno::ACCESS) => openat(
                parent,
                name,
                DIRECTORY_FLAGS | directory_search_flag(),
                Mode::empty(),
            )?,
            opened => opened?,
        };
        let stat = fstat(&fd)?;
        if stat.st_uid != geteuid().as_raw() || stat.st_mode & 0o077 != 0 {
            return Err(Errno::ACCESS.into());
        }
        if created && Mode::from_raw_mode(stat.st_mode) != DIRECTORY_MODE {
            match fchmod(&fd, DIRECTORY_MODE) {
                #[cfg(target_os = "linux")]
                Err(Errno::BADF) => chmod_search_descriptor(&fd)?,
                result => result?,
            }
        }
        Ok(fd)
    }

    #[cfg(target_os = "linux")]
    fn chmod_search_descriptor(fd: &OwnedFd) -> io::Result<()> {
        use std::os::fd::AsRawFd;

        // fchmod cannot act on O_PATH descriptors. Older Linux kernels also
        // lack chmodat(AT_EMPTY_PATH); procfs then addresses the pinned inode,
        // so neither fallback resolves the original directory name again.
        rustix::fs::chmodat(fd, "", DIRECTORY_MODE, AtFlags::EMPTY_PATH).or_else(|_| {
            rustix::fs::chmod(format!("/proc/self/fd/{}", fd.as_raw_fd()), DIRECTORY_MODE)
        })?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn directory_search_flag() -> OFlags {
        OFlags::PATH
    }

    #[cfg(target_os = "macos")]
    fn directory_search_flag() -> OFlags {
        // Darwin defines O_SEARCH as O_EXEC | O_DIRECTORY. The latter is in DIRECTORY_FLAGS.
        OFlags::from_bits_retain(0x4000_0000)
    }
}
