#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    fs, io,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use cortexkit_test_support::ScratchDir;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
#[cfg(unix)]
use rustix::process::{kill_process, test_kill_process, Pid, Signal};
use serde_json::{json, Value};
#[cfg(unix)]
use std::os::unix::fs::{symlink, PermissionsExt};

const CK: &str = env!("CARGO_BIN_EXE_ck-under-test");
const FIXTURE: &str = env!("CARGO_BIN_EXE_doctor-module-fixture");
const FIX_PROMPT: &str = "Apply this fix? [y/N]";
const DEEP_STOP_PROMPT: &str = "Stop blocked for up to 30 minutes to finish its deep checks? Agents that use it wait until it is back.";
const TITLE_PROMPT: &str = "Issue title (required):";
const DESCRIPTION_PROMPT: &str = "Issue description (optional):";
const ISSUE_PROMPT: &str = "File this report as a public issue";
const LIMIT: Duration = Duration::from_secs(25);
const SECONDARY_EXIT: u32 = if cfg!(windows) { 130 } else { 143 };
const PTY_SIZE: (u16, u16) = if cfg!(windows) { (150, 512) } else { (30, 180) };
type PtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;

struct Rig {
    root: ScratchDir,
    entries: BTreeMap<String, Value>,
}

impl Rig {
    fn new() -> Self {
        let root = ScratchDir::new("doctor-programs-cli");
        for path in [
            "data/doctor-fixture",
            "data/cortexkit",
            "config/cortexkit",
            "run",
            "bin",
            "home",
        ] {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        #[cfg(unix)]
        fs::set_permissions(root.join("run"), fs::Permissions::from_mode(0o700)).unwrap();
        #[cfg(windows)]
        for folder in ["run", "config/cortexkit", "data/cortexkit"] {
            subc_os::windows_acl::create_private_dir(&root.join(folder)).unwrap();
        }
        Self {
            root,
            entries: BTreeMap::new(),
        }
    }

    fn program(&mut self, id: &str, mut scenario: Value) -> PathBuf {
        let path = self.root.join(format!(
            "bin/fixture-{}{}",
            self.entries.len(),
            std::env::consts::EXE_SUFFIX
        ));
        fs::hard_link(FIXTURE, &path)
            .or_else(|_| fs::copy(FIXTURE, &path).map(|_| ()))
            .unwrap();
        scenario["record_stages"] = true.into();
        fs::write(self.stage(&path, "json"), scenario.to_string()).unwrap();
        self.entries
            .insert(id.into(), json!({"program": path, "enabled": false}));
        path
    }

    fn stage(&self, program: &Path, stage: &str) -> PathBuf {
        self.root.join("data/doctor-fixture").join(format!(
            "{}.{stage}",
            program.file_stem().unwrap().to_str().unwrap()
        ))
    }

    fn pid(&self, program: &Path, stage: &str) -> u32 {
        let path = self.stage(program, stage);
        until(|| fs::read_to_string(&path).ok()?.parse::<u32>().ok())
    }

    fn write_config(&self) {
        fs::write(
            self.root.join("config/cortexkit/subc.jsonc"),
            json!({"version": 1, "modules": self.entries}).to_string(),
        )
        .unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        self.command_as(Path::new(CK), args)
    }

    fn command_as(&self, path: &Path, args: &[&str]) -> Command {
        self.write_config();
        let mut command = Command::new(path);
        command
            .env_clear()
            .current_dir(self.root.path())
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("PATH", self.root.join("bin"))
            .env("TERM", "xterm-256color")
            .args(args);
        #[cfg(windows)]
        {
            command
                .env("USERPROFILE", self.root.join("home"))
                .env("APPDATA", self.root.join("config"))
                .env("LOCALAPPDATA", self.root.join("data"))
                .env("TEMP", self.root.path())
                .env("TMP", self.root.path());
            for name in ["SystemRoot", "WINDIR", "COMSPEC"] {
                if let Some(value) = std::env::var_os(name) {
                    command.env(name, value);
                }
            }
        }
        command
    }

    fn output(&self, args: &[&str]) -> Output {
        self.command(args)
            .env("CK_RESCUE_SESSION", "test")
            .output()
            .unwrap()
    }

    fn issue_tty(&self, api: &FakeApi) -> Tty {
        let mut command = self.command(&["doctor", "--issue"]);
        command
            .env("CK_DOCTOR_TEST_GITHUB_API", &api.base)
            .env("GH_HOST", "example.invalid");
        Tty::start(&command)
    }

    fn reports(&self) -> PathBuf {
        self.root
            .join("data")
            .join("cortexkit")
            .join("doctor")
            .join("reports")
    }

    #[cfg(target_os = "linux")]
    fn seed_install(&self) {
        for path in ["bin/ck-subc", "cmd/ck"] {
            let path = self.root.join("data/cortexkit").join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"installed executable placeholder").unwrap();
        }
    }

    fn log(&self, module: &str, text: &str) {
        let folder = self.root.join("data/cortexkit").join(module).join("logs");
        fs::create_dir_all(&folder).unwrap();
        fs::write(
            folder.join(format!("{module}.2026-09-05.log")),
            format!("2026-09-05T10:00:01.000Z INFO  doctor-fixture.fixture: {text}\n"),
        )
        .unwrap();
    }

    #[cfg(unix)]
    fn gh(&self, auth: u32, create: u32, sleep: bool) {
        let program = self.root.join("bin/gh");
        fs::write(
            &program,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$GH_HOST" "$@" > "gh.$1"
case "$1" in
    auth) exit {auth} ;;
    issue)
        for arg in "$@"; do
            [ "$previous" = "--body-file" ] && /bin/cat "$arg" > body.sent
            previous=$arg
        done
        {sleep}
        printf '%s\n' 'https://github.com/example/doctor-fixture/issues/1'
        exit {create} ;;
esac
exit 99
"#,
                sleep = if sleep { "/bin/sleep 2" } else { ":" }
            ),
        )
        .unwrap();
        fs::set_permissions(program, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(windows)]
    fn gh(&self, auth: u32, create: u32, sleep: bool) {
        fs::hard_link(FIXTURE, self.root.join("bin/gh.exe"))
            .or_else(|_| fs::copy(FIXTURE, self.root.join("bin/gh.exe")).map(|_| ()))
            .unwrap();
        fs::write(
            self.root.join("gh.json"),
            json!([auth, create, if sleep { 2000 } else { 0 }]).to_string(),
        )
        .unwrap();
    }
}

fn until<T>(mut read: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + LIMIT;
    loop {
        if let Some(value) = read() {
            return value;
        }
        assert!(
            Instant::now() < end,
            "test condition did not arrive within {LIMIT:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace('\r', "")
}

fn document(path: &Path) -> Value {
    let data = fs::read(path).unwrap();
    serde_json::from_slice(&data).unwrap_or_else(|error| panic!("{error}: {}", text(&data)))
}

fn gone(pid: u32) -> bool {
    #[cfg(unix)]
    {
        test_kill_process(Pid::from_raw(pid as i32).unwrap()) == Err(rustix::io::Errno::SRCH)
    }
    #[cfg(windows)]
    {
        !subc_jobobject::process_exists(pid)
    }
}

fn assert_gone(pid: u32) {
    assert!(gone(pid), "fixture {pid} still exists");
}

struct Tty {
    child: Option<Box<dyn Child + Send + Sync>>,
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<PtyWriter>,
    output: Arc<Mutex<Vec<u8>>>,
    reader: Option<thread::JoinHandle<io::Result<()>>>,
    stdout: PathBuf,
}

impl Tty {
    fn start(command: &Command) -> Self {
        let cwd = command.get_current_dir().unwrap();
        let stdout = cwd.join("stdout");
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: PTY_SIZE.0,
                cols: PTY_SIZE.1,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        #[cfg(unix)]
        let mut builder = {
            let mut builder = CommandBuilder::new("/bin/sh");
            builder.args(["-c", "exec \"$@\" > stdout", "doctor-test"]);
            builder
        };
        #[cfg(windows)]
        let mut builder = {
            let mut builder = CommandBuilder::new(FIXTURE);
            builder.arg("--redirect-doctor-cli");
            builder
        };
        builder.arg(command.get_program());
        builder.args(command.get_args());
        builder.cwd(cwd);
        builder.env_clear();
        for (name, value) in command.get_envs() {
            if let Some(value) = value {
                builder.env(name, value);
            }
        }
        let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
        #[cfg(windows)]
        let console_input = Arc::clone(&writer);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let child = pair.slave.spawn_command(builder).unwrap();
        drop(pair.slave);
        let output = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&output);
        let reader = thread::spawn(move || {
            #[cfg(windows)]
            let mut answered_cursor_query = false;
            let mut block = [0; 4096];
            loop {
                match reader.read(&mut block) {
                    Ok(0) => break,
                    Ok(n) => {
                        captured.lock().unwrap().extend_from_slice(&block[..n]);
                        #[cfg(windows)]
                        if !answered_cursor_query
                            && captured
                                .lock()
                                .unwrap()
                                .windows(4)
                                .any(|bytes| bytes == b"\x1b[6n")
                        {
                            // The Windows pseudo-terminal waits for a cursor position before it emits the prompt.
                            answered_cursor_query = true;
                            let mut input = console_input.lock().unwrap();
                            input.write_all(b"\x1b[1;1R")?;
                            input.flush()?;
                        }
                    }
                    #[cfg(unix)]
                    Err(error)
                        if error.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) =>
                    {
                        break
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        });
        Self {
            child: Some(child),
            master: Some(pair.master),
            writer: Some(writer),
            output,
            reader: Some(reader),
            stdout,
        }
    }

    #[cfg(unix)]
    fn pid(&self) -> Pid {
        Pid::from_raw(self.child.as_ref().unwrap().process_id().unwrap() as i32).unwrap()
    }

    #[cfg(unix)]
    fn signal(&self, signal: Signal) {
        kill_process(self.pid(), signal).unwrap();
    }

    #[cfg(unix)]
    fn foreground(&self) -> Option<u32> {
        self.master
            .as_ref()
            .unwrap()
            .process_group_leader()
            .and_then(Pid::from_raw)
            .map(|pid| pid.as_raw_pid() as u32)
    }

    fn interrupt(&mut self) {
        #[cfg(unix)]
        self.signal(Signal::INT);
        #[cfg(windows)]
        self.send("\x03");
    }

    fn secondary_interrupt(&mut self) {
        #[cfg(unix)]
        self.signal(Signal::TERM);
        // Windows has no SIGTERM delivery to a PID. Use the attached console.
        #[cfg(windows)]
        self.interrupt();
    }

    fn read_terminal(&self) -> String {
        let output = self.output.lock().unwrap();
        #[cfg(unix)]
        {
            text(&output)
        }
        #[cfg(windows)]
        {
            let mut terminal = vt100::Parser::new(PTY_SIZE.0, PTY_SIZE.1, 0);
            terminal.process(&output);
            terminal.screen().contents()
        }
    }

    fn expect(&self, needle: &str, count: usize) {
        let deadline = Instant::now() + LIMIT;
        loop {
            let dialogue = self.read_terminal();
            if dialogue.matches(needle).count() >= count {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "did not see {count} x {needle:?}\nterminal:\n{dialogue}\nstdout:\n{}",
                fs::read_to_string(&self.stdout).unwrap_or_default(),
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn send(&mut self, answer: &str) {
        #[cfg(windows)]
        let answer = answer.replace('\n', "\r");
        self.writer
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .write_all(answer.as_bytes())
            .unwrap();
    }

    fn answer_title(&mut self, title: &str, description: &str) {
        self.expect(TITLE_PROMPT, 1);
        self.send(&format!("{title}\n"));
        self.expect(DESCRIPTION_PROMPT, 1);
        self.send(&format!("{description}\n"));
    }

    fn finish(mut self, expected: u32) -> (String, String) {
        let deadline = Instant::now() + LIMIT;
        let status = loop {
            match self.child.as_mut().unwrap().try_wait() {
                Ok(Some(status)) => {
                    self.child.take();
                    break Ok(status);
                }
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    break Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "doctor CLI did not exit",
                    ))
                }
                Err(error) => break Err(error),
            }
        };
        // Reap a timed-out child before a test assertion can begin unwinding.
        self.stop_child();
        self.close_console();
        self.reader
            .take()
            .unwrap()
            .join()
            .unwrap()
            .expect("read doctor console");
        let stdout = text(&fs::read(&self.stdout).unwrap());
        let dialogue = self.read_terminal();
        let status = status
            .unwrap_or_else(|error| panic!("{error}\nterminal:\n{dialogue}\nstdout:\n{stdout}"));
        assert_eq!(
            status.exit_code(),
            expected,
            "terminal:\n{dialogue}\nstdout:\n{stdout}"
        );
        (stdout, dialogue)
    }

    fn stop_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn close_console(&mut self) {
        drop(self.writer.take());
        drop(self.master.take());
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        self.stop_child();
        self.close_console();
    }
}

enum ApiReply {
    Status(u16, &'static str),
    Redirect,
    Timeout,
}

struct FakeApi {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
}

impl FakeApi {
    fn new(reply: ApiReply) -> Self {
        use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let redirect = format!("{base}/repos/example/public");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let recorded = Arc::clone(&requests);
        let stopped = Arc::clone(&stop);
        let task = thread::spawn(move || {
            let mut held = Vec::new();
            while !stopped.load(Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(error) => panic!("accept public API request: {error}"),
                };
                // On Windows an accepted socket inherits the listener's
                // non-blocking mode (Unix sockets don't), so a read issued
                // before the request bytes arrive fails with WouldBlock.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") && request.len() < 8192 {
                    let mut byte = [0];
                    if stream.read(&mut byte).unwrap() == 0 {
                        break;
                    }
                    request.push(byte[0]);
                }
                recorded.lock().unwrap().push(text(&request));
                let response = match reply {
                    ApiReply::Status(code, body) => format!("HTTP/1.1 {code} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()),
                    ApiReply::Redirect if text(&request).contains(" /repos/example/public ") => "HTTP/1.1 200 OK\r\nContent-Length: 17\r\nConnection: close\r\n\r\n{\"private\":false}".to_owned(),
                    ApiReply::Redirect => format!("HTTP/1.1 302 Found\r\nLocation: {redirect}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                    ApiReply::Timeout => {
                        held.push(stream);
                        continue;
                    }
                };
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        Self {
            base,
            requests,
            stop,
            task: Some(task),
        }
    }

    fn public() -> Self {
        Self::new(ApiReply::Status(200, r#"{"private":false}"#))
    }

    fn assert_requests(&self, count: usize) {
        let requests = self.requests.lock().unwrap();
        assert_eq!(requests.len(), count, "{requests:?}");
        for request in requests.iter() {
            let request = request.to_ascii_lowercase();
            assert!(
                request.starts_with("get /repos/example/doctor-fixture http/1.1\n"),
                "{request}"
            );
            assert!(
                request.contains(concat!("user-agent: ck/", env!("CARGO_PKG_VERSION"))),
                "{request}"
            );
            assert!(!request.contains("authorization:"), "{request}");
        }
    }
}

impl Drop for FakeApi {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        // A failed request can panic on the server thread. Do not turn an
        // already-failing test into a process abort by panicking in cleanup.
        if let Some(Err(failure)) = self.task.take().map(thread::JoinHandle::join) {
            if !thread::panicking() {
                std::panic::resume_unwind(failure);
            }
        }
    }
}

fn report_paths(stdout: &str) -> Vec<PathBuf> {
    stdout
        .lines()
        .filter_map(|line| {
            let (_, path) = line.rsplit_once(" [")?;
            let path = path.strip_suffix(']')?;
            (path.ends_with(".md") || path.ends_with(".json")).then(|| PathBuf::from(path))
        })
        .collect()
}

#[test]
fn doctor_issue_conflicts_and_help() {
    let rig = Rig::new();
    let report = rig.root.join("usage.json");
    for (flags, refused) in [
        (&["--fix"][..], "--fix"),
        (&["--json"][..], "--json"),
        (&["--agent"][..], "--agent"),
        (&["--forget"][..], "--forget"),
        (&["--subc", "overridden.json"][..], "--subc"),
    ] {
        let output = rig
            .command(&["doctor", "--issue", "--report"])
            .arg(&report)
            .args(flags)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(text(&output.stderr).contains(&format!("cannot be used with {refused}")));
        assert!(!report.exists());
    }
    let output = rig
        .command(&["doctor", "--issue"])
        .env("SUBC_CONNECTION_FILE", "local.json")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("cannot be used with SUBC_CONNECTION_FILE"));
    let ckni = rig
        .root
        .join(format!("bin/ckni{}", std::env::consts::EXE_SUFFIX));
    fs::hard_link(CK, &ckni)
        .or_else(|_| fs::copy(CK, &ckni).map(|_| ()))
        .unwrap();
    let output = rig
        .command_as(&ckni, &["doctor", "--issue"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("cannot be used with ckni"));
    let output = rig.output(&["help", "doctor"]);
    assert!(output.status.success());
    let help = text(&output.stdout);
    assert!(help.contains("ck doctor --issue [--report <file>]"));
    assert!(help
        .contains("--issue          save sanitized module reports and ask before filing publicly"));
}

#[test]
fn doctor_module_prompt_uses_the_terminal_with_stdout_redirected() {
    let mut rig = Rig::new();
    let program = rig.program("safe", json!({"record_fix": true, "clean_after_fix": true}));
    let mut tty = Tty::start(&rig.command(&["doctor", "--fix"]));
    tty.expect(FIX_PROMPT, 1);
    let plan = tty.read_terminal();
    assert!(plan.contains("safe: The fixture state needs repair."));
    assert!(plan.contains("Reset the fixture state.\n  - Verify the fixture state."));
    assert!(plan.contains("No module stop is needed."));
    tty.send("n\n");
    let (stdout, _) = tty.finish(1);
    assert!(stdout.contains("module program safe:"));
    assert!(stdout.contains("Reset the fixture state. (run `ck doctor --fix` to apply)"));
    assert!(stdout.contains("Module fix module.failed [safe]: declined"));
    for only_tty in [FIX_PROMPT, "Verify the fixture state.", "Nothing to fix."] {
        assert!(!stdout.contains(only_tty), "{stdout}");
    }
    assert!(!rig.stage(&program, "fix.started").exists());

    let mut tty = Tty::start(&rig.command(&["doctor", "--fix"]));
    tty.expect(FIX_PROMPT, 1);
    tty.send("yes\n");
    let (stdout, _) = tty.finish(1);
    assert!(stdout.contains("Module fix module.failed [safe]: applied; fixed"));
    assert!(!stdout.contains("module program safe:"));
    assert!(rig.stage(&program, "recheck.finished").exists());
}

#[test]
fn doctor_module_fix_refuses_rescue_terminal_and_missing_terminal() {
    let mut rig = Rig::new();
    let program = rig.program("safe", json!({"record_fix": true}));
    let mut cmd = rig.command(&["doctor", "--fix", "--json"]);
    cmd.env("CK_RESCUE_SESSION", "1");
    let tty = Tty::start(&cmd);
    let (stdout, dialogue) = tty.finish(1);
    let result: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        result["fix"]["module_fixes"][0]["skip_reason"],
        "no_terminal"
    );
    assert!(!dialogue.contains(FIX_PROMPT));
    assert!(!rig.stage(&program, "fix.started").exists());

    #[cfg(target_os = "linux")]
    {
        let mut cmd = rig.command_as(
            Path::new("/usr/bin/setsid"),
            &[
                "--wait",
                "/bin/sh",
                "-c",
                "exec 0<&-; exec \"$@\"",
                "test",
                CK,
                "doctor",
                "--fix",
                "--json",
            ],
        );
        let output = cmd.output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        let document: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            document["fix"]["module_fixes"][0]["skip_reason"],
            "no_terminal"
        );
        assert!(!rig.stage(&program, "fix.started").exists());
    }
    #[cfg(windows)]
    {
        let mut command = rig.command(&["doctor", "--fix"]);
        command.env("CK_TEST_REDIRECT_STDIN", "1");
        let (out, dialogue) = Tty::start(&command).finish(1);
        assert!(
            out.contains("skipped no_terminal; the answer must be typed in a console window"),
            "{out}"
        );
        assert!(!dialogue.contains(FIX_PROMPT));
        assert!(!rig.stage(&program, "fix.started").exists());
    }
}

#[test]
fn doctor_signal_during_fix_finishes_and_writes_requested_report() {
    for (exit, save) in [(130, false), (SECONDARY_EXIT, true)] {
        let mut rig = Rig::new();
        let program = rig.program("slow", json!({"fix": "sleep", "record_fix": true}));
        let mut command = rig.command(&["doctor", "--fix", "--json"]);
        let report = rig.root.join("requested.json");
        if save {
            command.arg("--report").arg(&report);
        }
        let mut tty = Tty::start(&command);
        tty.expect(FIX_PROMPT, 1);
        tty.send("y\n");
        let pid = rig.pid(&program, "fix.started");
        if save {
            tty.secondary_interrupt();
        } else {
            tty.interrupt();
        }
        tty.expect("waiting for a module fix", 1);
        let (stdout, _) = tty.finish(exit);
        let result: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(
            result["fix"]["module_fixes"][0]["result"]["outcome"],
            "fixed"
        );
        assert!(!rig.stage(&program, "recheck.started").exists());
        assert_gone(pid);
        if save {
            assert_eq!(document(&report)["fix"], result["fix"]);
        }
    }
}

#[test]
fn doctor_partial_registration_keeps_checks_and_the_sigint_listener() {
    for signal in [false, true] {
        let mut rig = Rig::new();
        let program = rig.program("slow", json!({"check_sleep_ms": 1000}));
        let mut command = rig.command(&["doctor", "--fix", "--json"]);
        command.env("CK_TEST_FAIL_SIGTERM_REGISTRATION", "1");
        let mut tty = Tty::start(&command);
        let pid = rig.pid(&program, "check.started");
        if signal {
            tty.interrupt();
            tty.expect("waiting for a module check", 1);
        }
        let (stdout, dialogue) = tty.finish(if signal { 130 } else { 1 });
        let result: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(result["modules"][0]["outcome"], "findings");
        assert_eq!(
            result["fix"]["module_fixes"][0]["skip_reason"],
            if signal {
                "interrupted"
            } else {
                "signal_setup_failed"
            }
        );
        assert!(!dialogue.contains(FIX_PROMPT));
        assert!(!rig.stage(&program, "fix.started").exists());
        assert_gone(pid);
    }
}

#[test]
fn doctor_interrupt_latches_across_pool_jobs_and_during_a_second_pass() {
    let mut rig = Rig::new();
    let early = rig.program("early", json!({"check": "clean"}));
    let slow = rig.program("slow", json!({"check": "clean", "check_sleep_ms": 1600}));
    let mut tty = Tty::start(&rig.command(&["doctor", "--json"]));
    let early_pid = rig.pid(&early, "check.finished");
    let slow_pid = rig.pid(&slow, "check.started");
    tty.secondary_interrupt();
    tty.expect("waiting for a module check", 1);
    let (stdout, _) = tty.finish(SECONDARY_EXIT);
    let result: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(result["modules"].as_array().unwrap().len(), 2);
    assert_gone(early_pid);
    assert_gone(slow_pid);

    let mut rig = Rig::new();
    let program = rig.program(
        "slow",
        json!({"clean_after_fix": true, "check_sleep_ms": 1000}),
    );
    let mut tty = Tty::start(&rig.command(&["doctor", "--json", "--fix"]));
    tty.expect(FIX_PROMPT, 1);
    tty.send("y\n");
    let pid = rig.pid(&program, "recheck.started");
    tty.secondary_interrupt();
    tty.expect("waiting for a module check", 1);
    let (stdout, dialogue) = tty.finish(SECONDARY_EXIT);
    let result: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        result["fix"]["module_fixes"][0]["result"]["outcome"],
        "fixed"
    );
    assert_eq!(result["modules"][0]["outcome"], "clean");
    assert_eq!(dialogue.matches(FIX_PROMPT).count(), 1);
    assert_gone(pid);
}

#[test]
fn doctor_issue_saves_private_unique_reports_with_isolated_log_tails() {
    let mut rig = Rig::new();
    let long = format!("{}x", "é".repeat(127));
    let only_a = "First fixture allowlisted marker";
    let only_b = "Second fixture allowlisted marker";
    rig.program(&long, json!({}));
    rig.program("alpha", json!({"log_template": only_a}));
    rig.program("other", json!({"log_template": only_b}));
    rig.program("no-log", json!({}));
    rig.program("runner-only", json!({"check": "crash"}));
    rig.log("alpha", only_a);
    rig.log("other", only_b);
    let api = FakeApi::public();
    rig.gh(0, 0, false);
    #[cfg(unix)]
    let mut command = rig.command_as(
        Path::new("/bin/sh"),
        &[
            "-c",
            "umask 0477; exec \"$@\"",
            "test",
            CK,
            "doctor",
            "--issue",
        ],
    );
    #[cfg(windows)]
    let mut command = rig.command(&["doctor", "--issue"]);
    command
        .env("CK_DOCTOR_TEST_GITHUB_API", &api.base)
        .env("CK_RESCUE_SESSION", "1");
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    let paths = report_paths(&stdout);
    assert_eq!(paths.len(), 4, "{stdout}");
    assert!(
        stdout.contains(&format!("{long}: saved_only [")),
        "{stdout}"
    );
    assert!(!stdout.contains("runner-only: saved"));
    for path in &paths {
        assert_eq!(path.parent().unwrap(), rig.reports());
        assert_private(path, false);
        let name = path.file_name().unwrap().to_str().unwrap();
        let (stamp, random) = name.strip_suffix(".md").unwrap().split_once('-').unwrap();
        assert_eq!(stamp.len(), 16);
        assert!(stamp.ends_with('Z'));
        assert_eq!(random.len(), 16);
        assert!(random.bytes().all(|b| b.is_ascii_hexdigit()));
    }
    for path in [rig.reports(), rig.reports().parent().unwrap().to_owned()] {
        assert_private(&path, true);
    }
    let a = fs::read_to_string(&paths[0]).unwrap();
    let no_log = fs::read_to_string(&paths[1]).unwrap();
    let b = fs::read_to_string(&paths[2]).unwrap();
    assert!(a.contains(only_a) && !a.contains(only_b), "{a}");
    assert!(b.contains(only_b) && !b.contains(only_a), "{b}");
    assert!(
        !no_log.contains("text omitted") && !no_log.contains("could not"),
        "{no_log}"
    );
    let again = report_paths(&text(&command.output().unwrap().stdout));
    assert_eq!(again.len(), 4);
    assert!(again.iter().all(|path| !paths.contains(path)));
    api.assert_requests(0);
    assert!(!rig.root.join("gh.auth").exists());
}

#[test]
fn doctor_issue_refuses_a_link_and_never_tightens_existing_report_dirs() {
    for link in [true, false] {
        let mut rig = Rig::new();
        rig.program("reportable", json!({}));
        let parent = rig.reports();
        let parent = parent.parent().unwrap();
        #[cfg(windows)]
        subc_os::windows_acl::create_private_dir(parent).unwrap();
        #[cfg(unix)]
        fs::create_dir(parent).unwrap();
        #[cfg(unix)]
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).unwrap();
        let target = if link {
            rig.root.join("unrelated")
        } else {
            rig.reports()
        };
        fs::create_dir(&target).unwrap();
        #[cfg(unix)]
        let original_mode = if link { 0o700 } else { 0o750 };
        #[cfg(unix)]
        fs::set_permissions(&target, fs::Permissions::from_mode(original_mode)).unwrap();
        #[cfg(windows)]
        if link {
            subc_os::windows_acl::create_private_dir(&target).unwrap();
        } else {
            subc_os::windows_acl::test_support::grant_everyone(&target);
        }
        if link {
            #[cfg(unix)]
            symlink(&target, rig.reports()).unwrap();
            #[cfg(windows)]
            {
                let result = Command::new("cmd.exe")
                    .args(["/d", "/c", "mklink", "/j"])
                    .arg(rig.reports())
                    .arg(&target)
                    .output()
                    .unwrap();
                assert!(
                    result.status.success(),
                    "mklink /j: {}",
                    text(&result.stderr)
                );
            }
        }
        let result = rig.output(&["doctor", "--issue"]);
        assert_eq!(result.status.code(), Some(1));
        let stdout = text(&result.stdout);
        assert!(
            stdout.ends_with("reportable: skipped save_failed\n"),
            "{stdout}"
        );
        assert!(fs::read_dir(&target).unwrap().next().is_none());
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(target).unwrap().permissions().mode() & 0o777,
            original_mode
        );
        #[cfg(windows)]
        if !link {
            assert!(subc_os::windows_acl::verify_private_dir(&target).is_err());
        }
    }
}

fn assert_private(path: &Path, directory: bool) {
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        if directory { 0o700 } else { 0o600 }
    );
    #[cfg(windows)]
    subc_os::windows_acl::test_support::assert_owner_only(path, directory, true);
}

#[test]
fn doctor_issue_invalid_manifest_saves_local_report_without_raw_log_text() {
    let mut rig = Rig::new();
    rig.program(
        "broken-manifest",
        json!({"manifest": "invalid_repository", "log_template": "RAW-LOG-CANARY"}),
    );
    rig.log("broken-manifest", "RAW-LOG-CANARY");
    let api = FakeApi::public();
    rig.gh(0, 0, false);
    let tty = rig.issue_tty(&api);
    let (stdout, terminal) = tty.finish(1);
    assert!(
        stdout.contains("broken-manifest: skipped manifest_invalid ["),
        "{stdout}"
    );
    assert!(
        stdout.contains("The module's manifest was invalid. This report is local only."),
        "{stdout}"
    );
    assert!(!terminal.contains(TITLE_PROMPT));
    let paths = report_paths(&stdout);
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].parent().unwrap(), rig.reports());
    assert_eq!(paths[0].extension().unwrap(), "json");
    assert_private(&paths[0], false);
    let body = fs::read_to_string(&paths[0]).unwrap();
    assert!(!body.contains("RAW-LOG-CANARY"), "{body}");
    let note: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(note["schema"], "ck.doctor-report/v1");
    assert_eq!(note["module_id"], "broken-manifest");
    assert_eq!(note["check"]["outcome"], "ok");
    assert_eq!(note["findings"][0]["id"], "module.failed");
    assert!(note.get("repository").is_none());
    assert_eq!(note["logs"], json!([]));
    api.assert_requests(0);
    assert!(!rig.root.join("gh.auth").exists());
}

#[test]
fn doctor_issue_invalid_manifest_cannot_save_a_nonconforming_module_id() {
    let mut rig = Rig::new();
    rig.program("parent:child", json!({"manifest": "invalid_repository"}));
    let api = FakeApi::public();
    rig.gh(0, 0, false);
    let tty = rig.issue_tty(&api);
    let (stdout, terminal) = tty.finish(1);
    assert!(stdout.contains("module.failed"), "{stdout}");
    assert!(
        stdout.contains("parent:child: skipped save_failed\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("The module ID is not valid for the report format."),
        "{stdout}"
    );
    assert!(!terminal.contains(TITLE_PROMPT));
    assert!(!rig.reports().exists());
    api.assert_requests(0);
    assert!(!rig.root.join("gh.auth").exists());
}

#[test]
fn doctor_issue_awaits_manifest_batch_and_skips_replaced_programs() {
    let mut rig = Rig::new();
    let changed = rig.program("alpha-changed", json!({}));
    let slow = rig.program(
        "beta-hanging",
        json!({"manifest": "hanging", "check_sleep_ms": 1200}),
    );
    let valid = rig.program("gamma-valid", json!({}));
    let api = FakeApi::public();
    let mut tty = rig.issue_tty(&api);
    let pid = rig.pid(&changed, "check.finished");
    until(|| gone(pid).then_some(()));
    fs::rename(&changed, changed.with_extension("saved")).unwrap();
    fs::copy(FIXTURE, &changed).unwrap();
    let hanging_pid = rig.pid(&slow, "manifest.started");
    let valid_pid = rig.pid(&valid, "manifest.finished");
    assert!(!tty.read_terminal().contains(TITLE_PROMPT));
    tty.expect(TITLE_PROMPT, 1);
    assert_gone(hanging_pid);
    assert_gone(valid_pid);
    assert!(!rig.stage(&changed, "manifest.started").exists());
    tty.answer_title("Ready", "");
    tty.expect(ISSUE_PROMPT, 1);
    tty.send("n\n");
    let (stdout, _) = tty.finish(1);
    assert!(
        stdout.contains("alpha-changed: skipped program_changed\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("beta-hanging: skipped manifest_timed_out\n"),
        "{stdout}"
    );
    assert!(stdout.contains("gamma-valid: declined ["), "{stdout}");
    api.assert_requests(1);
}

#[test]
fn doctor_issue_refuses_non_public_api_responses_without_gh() {
    for response in [
        ApiReply::Status(200, r#"{"private":true}"#),
        ApiReply::Status(404, "{}"),
        ApiReply::Redirect,
        ApiReply::Timeout,
    ] {
        let mut rig = Rig::new();
        rig.program("public-test", json!({}));
        #[cfg(target_os = "linux")]
        rig.seed_install();
        let _daemon = WireDaemon::start(&rig, None, StartReply::Ack);
        rig.gh(0, 0, false);
        let api = FakeApi::new(response);
        let mut tty = rig.issue_tty(&api);
        tty.answer_title("A title", "A description");
        let (stdout, dialogue) = tty.finish(u32::from(!cfg!(target_os = "linux")));
        assert!(stdout.contains("public-test: not_public ["), "{stdout}");
        assert_eq!(report_paths(&stdout).len(), 1);
        assert!(!dialogue.contains(ISSUE_PROMPT));
        api.assert_requests(1);
        assert!(!rig.root.join("gh.auth").exists());
    }
}

#[test]
fn doctor_issue_file_and_fallback_use_the_preview_and_hide_private_id() {
    const PRIVATE: &str = "person-doctor@example.invalid";
    for (auth, create, outcome) in [
        (
            0,
            0,
            "filed https://github.com/example/doctor-fixture/issues/1",
        ),
        (73, 0, "url_printed"),
        (0, 74, "url_printed"),
    ] {
        let mut rig = Rig::new();
        rig.program(PRIVATE, json!({"private_ref": "private-branch-canary", "private_path": "/tmp/private-filename"}));
        #[cfg(target_os = "linux")]
        rig.seed_install();
        let _daemon = WireDaemon::start(&rig, None, StartReply::Ack);
        let api = FakeApi::public();
        rig.gh(auth, create, false);
        if auth != 0 {
            let output = rig
                .command(&["doctor", "--issue"])
                .env("CK_RESCUE_SESSION", "1")
                .env("CK_DOCTOR_TEST_GITHUB_API", &api.base)
                .output()
                .unwrap();
            let stdout = text(&output.stdout);
            assert!(
                stdout.contains(&format!("{PRIVATE}: saved_only [")),
                "{stdout}"
            );
            let body = fs::read_to_string(&report_paths(&stdout)[0]).unwrap();
            assert!(!body.contains(PRIVATE), "{body}");
            api.assert_requests(0);
            assert!(!rig.root.join("gh.auth").exists());
        }
        let mut terminal = rig.issue_tty(&api);
        terminal.expect(TITLE_PROMPT, 1);
        terminal.send("\n");
        terminal.expect(TITLE_PROMPT, 2);
        terminal.answer_title(&format!("Issue for {PRIVATE}"), "This state needs repair");
        terminal.expect(ISSUE_PROMPT, 1);
        let shown = terminal.read_terminal();
        let shown = shown.split_once("\nTitle: ").unwrap().1;
        let (title, rest) = shown.split_once('\n').unwrap();
        let (preview, _) = rest
            .split_once("\nFile this report as a public issue")
            .unwrap();
        assert!(
            title.starts_with("Issue for ") && !title.contains(PRIVATE),
            "{shown}"
        );
        assert!(
            !preview.contains(PRIVATE)
                && !preview.contains("private-branch-canary")
                && !preview.contains("private-filename"),
            "{preview}"
        );
        terminal.send("Y\n");
        let (stdout, dialogue) =
            terminal.finish(u32::from(create != 0 || !cfg!(target_os = "linux")));
        assert!(
            stdout.contains(&format!("{PRIVATE}: {outcome} [")),
            "{stdout}"
        );
        let paths = report_paths(&stdout);
        assert_eq!(paths.len(), 1);
        let saved = fs::read_to_string(&paths[0]).unwrap();
        #[cfg(unix)]
        assert_eq!(saved, preview);
        #[cfg(windows)]
        assert_eq!(saved.trim_end(), preview.trim_end());
        api.assert_requests(1);
        assert_eq!(
            fs::read_to_string(rig.root.join("gh.auth")).unwrap(),
            "github.com\nauth\nstatus\n--hostname\ngithub.com\n"
        );
        if auth == 0 {
            let args = fs::read_to_string(rig.root.join("gh.issue")).unwrap();
            assert_eq!(args, format!("github.com\nissue\ncreate\n--repo\ngithub.com/example/doctor-fixture\n--title\n{title}\n--body-file\n{}\n", paths[0].display()));
            assert_eq!(
                fs::read_to_string(rig.root.join("body.sent")).unwrap(),
                saved
            );
        } else {
            assert!(!rig.root.join("gh.issue").exists());
        }
        if outcome == "url_printed" {
            let url = dialogue
                .lines()
                .find(|line| line.contains("/issues/new?"))
                .unwrap();
            assert!(url.contains("?title=Issue%20for%20") && url.contains("&body="));
            assert!(
                !url.contains(PRIVATE) && !url.contains("person-doctor%40"),
                "{url}"
            );
        }
    }
}

#[test]
fn doctor_issue_interrupted_during_manifest_batch_skips_all_filing() {
    let mut rig = Rig::new();
    let a = rig.program("alpha", json!({}));
    let b = rig.program("beta", json!({"manifest_sleep_ms": 1300}));
    let api = FakeApi::public();
    rig.gh(0, 0, false);
    let mut tty = rig.issue_tty(&api);
    let a_pid = rig.pid(&a, "manifest.finished");
    let b_pid = rig.pid(&b, "manifest.started");
    tty.secondary_interrupt();
    tty.expect("waiting for a module manifest", 1);
    let (stdout, terminal) = tty.finish(SECONDARY_EXIT);
    assert!(
        stdout.ends_with("alpha: skipped interrupted\nbeta: skipped interrupted\n"),
        "{stdout}"
    );
    assert!(!terminal.contains(TITLE_PROMPT));
    assert!(!rig.reports().exists());
    assert_gone(a_pid);
    assert_gone(b_pid);
    api.assert_requests(0);
    assert!(!rig.root.join("gh.auth").exists());
}

#[test]
fn doctor_issue_interrupted_during_gh_finishes_the_file_and_skips_the_sibling() {
    let mut rig = Rig::new();
    rig.program("alpha", json!({}));
    rig.program("beta", json!({}));
    let api = FakeApi::public();
    rig.gh(0, 0, true);
    let mut tty = rig.issue_tty(&api);
    tty.answer_title("First issue", "");
    tty.expect(ISSUE_PROMPT, 1);
    tty.send("y\n");
    until(|| rig.root.join("body.sent").exists().then_some(()));
    tty.secondary_interrupt();
    tty.expect("waiting for gh issue create", 1);
    let (stdout, terminal) = tty.finish(SECONDARY_EXIT);
    assert!(
        stdout.contains("alpha: filed https://github.com/example/doctor-fixture/issues/1 ["),
        "{stdout}"
    );
    assert!(stdout.ends_with("beta: skipped interrupted\n"), "{stdout}");
    assert_eq!(terminal.matches(TITLE_PROMPT).count(), 1);
    api.assert_requests(1);
}

#[derive(Clone, Copy)]
enum StartReply {
    Ack,
    Unanswered,
}

#[derive(Default)]
struct WireEvents {
    calls: Mutex<Vec<subc_control::ClientControlRequest>>,
    hold_stop: std::sync::atomic::AtomicBool,
    stop_ack: tokio::sync::Notify,
}

struct WireDaemon {
    _runtime: tokio::runtime::Runtime,
    events: Arc<WireEvents>,
}

impl WireDaemon {
    fn start(
        rig: &Rig,
        entry: Option<subc_control::SupervisorEntry>,
        start_reply: StartReply,
    ) -> Self {
        use subc_transport::{
            generate_daemon_id, generate_key, write_atomic, ConnectionInfo, Endpoint,
            CONNECTION_FILE_NAME, SCHEMA_VERSION,
        };

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let connection = ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: Some(subc_protocol::PROTOCOL_VERSION),
            endpoints: vec![Endpoint {
                host: "127.0.0.1".into(),
                port: listener.local_addr().unwrap().port(),
            }],
            key: generate_key().unwrap(),
            daemon_id: generate_daemon_id().unwrap(),
            pid: std::process::id(),
            daemon_ver: "doctor-test-daemon".into(),
        };
        write_atomic(rig.root.join("run").join(CONNECTION_FILE_NAME), &connection).unwrap();
        let events = Arc::new(WireEvents::default());
        let worker_events = Arc::clone(&events);
        runtime.spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let connection = connection.clone();
                let entry = entry.clone();
                let events = Arc::clone(&worker_events);
                tokio::spawn(Self::serve(stream, connection, entry, start_reply, events));
            }
        });
        Self {
            _runtime: runtime,
            events,
        }
    }

    async fn serve(
        mut stream: tokio::net::TcpStream,
        connection: subc_transport::ConnectionInfo,
        mut entry: Option<subc_control::SupervisorEntry>,
        start_reply: StartReply,
        events: Arc<WireEvents>,
    ) {
        use subc_control::{
            ClientControlRequest as Request, ClientControlResponse as Response, StderrCaptureState,
            StderrTail, TerminalHistory,
        };
        use subc_daemon::{read_frame, write_frame, Frame};
        use subc_protocol::{FrameType, PROTOCOL_VERSION};

        subc_transport::authenticate_server(
            &mut stream,
            &connection.key,
            &connection.daemon_id,
            &connection.daemon_ver,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        while let Ok(Some(frame)) = read_frame(&mut stream).await {
            let request: Request = serde_json::from_slice(&frame.body).unwrap();
            events.calls.lock().unwrap().push(request.clone());
            let reply = match request {
                Request::ServerDescribe {} => Response::ServerDescribe {
                    protocol_ver: PROTOCOL_VERSION,
                    subc_ops: vec![],
                    capabilities: vec![],
                    connected_clients: 1,
                    counters: None,
                    build_git_sha: None,
                    build_lock_digest: None,
                    capability_requirements: vec![],
                    machine_id: None,
                    config_path: None,
                },
                Request::SupervisorList {} => Response::SupervisorList {
                    generation: 1,
                    modules: entry.iter().cloned().collect(),
                    daemon: None,
                    agent_commands: None,
                },
                Request::SupervisorHealth {} => Response::SupervisorHealth {
                    generation: 1,
                    modules: vec![],
                },
                Request::SupervisorTerminals { module_id } => Response::SupervisorTerminals {
                    module_id,
                    terminals: TerminalHistory {
                        daemon_started_at_ms: 0,
                        entries: vec![],
                        dropped: 0,
                        journal_skipped_lines: 0,
                        journal_read_errors: 0,
                        journal_write_failures: 0,
                    },
                },
                Request::SupervisorStderrTail { module_id, .. } => Response::SupervisorStderrTail {
                    module_id,
                    tail: StderrTail {
                        capture: StderrCaptureState::Captured,
                        entries: vec![],
                        dropped_lines: 0,
                    },
                },
                Request::SupervisorSetEnabled { module_id, enabled } => {
                    let row = entry.as_mut().expect("daemon has a module to stop");
                    assert_eq!(row.module_id, module_id);
                    assert_ne!(row.enabled, enabled);
                    if enabled && matches!(start_reply, StartReply::Unanswered) {
                        std::future::pending::<()>().await;
                    }
                    if !enabled && events.hold_stop.load(std::sync::atomic::Ordering::Relaxed) {
                        events.stop_ack.notified().await;
                    }
                    row.enabled = enabled;
                    row.live = enabled;
                    row.state = if enabled { "running" } else { "disabled" }.into();
                    Response::SupervisorAck {
                        module_id,
                        applied: true,
                    }
                }
                other => panic!("unexpected doctor RPC: {other:?}"),
            };
            let reply = Frame::build_with_version(
                frame.header.ver,
                FrameType::Response,
                frame.header.flags,
                0,
                0,
                frame.header.corr,
                serde_json::to_vec(&reply).unwrap(),
            )
            .unwrap();
            write_frame(&mut stream, &reply).await.unwrap();
        }
    }

    fn enabled_calls(&self) -> Vec<bool> {
        use subc_control::ClientControlRequest::SupervisorSetEnabled;

        self.events
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter_map(|call| match call {
                SupervisorSetEnabled { enabled, .. } => Some(*enabled),
                _ => None,
            })
            .collect()
    }
}

fn supervisor_row(id: &str, state: &str, live: bool) -> subc_control::SupervisorEntry {
    subc_control::SupervisorEntry::new(
        id,
        state,
        true,
        live,
        subc_control::SupervisorHealthStatus::Unknown,
    )
    .with_drain_timeout_ms(Some(200))
}

#[test]
fn doctor_full_json_reports_the_mode_that_ran_without_stopping() {
    let mut rig = Rig::new();
    let old = rig.program("old", json!({"check": "clean"}));
    let blocked = rig.program(
        "blocked",
        json!({"full": "findings", "finding": "store.in_use"}),
    );
    rig.entries.insert(
        "skipped".into(),
        json!({"program": old, "protocol": "none"}),
    );
    let daemon = WireDaemon::start(
        &rig,
        Some(supervisor_row("blocked", "running", true)),
        StartReply::Ack,
    );
    let (stdout, dialogue) = Tty::start(&rig.command(&["doctor", "--full", "--json"])).finish(1);
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["schema"], "ck.doctor.report/v1");
    assert_eq!(
        report["modules"],
        json!([
            {"id":"blocked","outcome":"findings","findings":1,"detail":null,"mode":"full"},
            {"id":"old","outcome":"clean","findings":0,"detail":"full_not_supported","mode":"quick"},
            {"id":"skipped","outcome":"not_asked","findings":0,"detail":"protocol_none"},
        ])
    );
    assert!(report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["id"] == "store.in_use"));
    assert!(!dialogue.contains(DEEP_STOP_PROMPT), "{dialogue}");
    assert!(daemon.enabled_calls().is_empty());
    assert!(rig.stage(&old, "check.finished").is_file());
    assert!(!rig.stage(&blocked, "check.started").exists());
}

#[test]
fn doctor_full_noninteractive_never_stops() {
    let mut rig = Rig::new();
    rig.program(
        "blocked",
        json!({"full": "findings", "finding": "store.in_use"}),
    );
    let daemon = WireDaemon::start(
        &rig,
        Some(supervisor_row("blocked", "running", true)),
        StartReply::Ack,
    );
    let output = rig.output(&["doctor", "--full"]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output.stderr));
    assert!(text(&output.stdout).contains("store.in_use [blocked]"));
    assert!(!text(&output.stderr).contains(DEEP_STOP_PROMPT));
    assert!(daemon.enabled_calls().is_empty());
}

#[test]
fn doctor_full_runs_one_at_a_time_and_uses_the_check_mode_for_fixes() {
    let mut rig = Rig::new();
    let first = rig.program(
        "a",
        json!({"full": "findings", "wait_stage": "full", "clean_after_fix": true}),
    );
    let later = rig.program("b", json!({"clean_after_fix": true}));
    let quick = rig.output(&["doctor", "--json"]);
    assert_eq!(quick.status.code(), Some(1), "{}", text(&quick.stderr));
    let quick: Value = serde_json::from_slice(&quick.stdout).unwrap();
    assert_eq!(
        quick["modules"],
        json!([
            {"id":"a","outcome":"findings","findings":1,"detail":null},
            {"id":"b","outcome":"findings","findings":1,"detail":null},
        ])
    );
    assert!(rig.stage(&first, "check.finished").is_file());
    assert!(!rig.stage(&first, "full.started").exists());
    fs::remove_file(rig.stage(&later, "check.started")).unwrap();

    let mut tty = Tty::start(&rig.command(&["doctor", "--full", "--fix"]));
    rig.pid(&first, "full.started");
    tty.expect("of up to 30 min", 2);
    let dialogue = tty.read_terminal();
    let mut progress = dialogue
        .lines()
        .filter(|line| line.starts_with("Checking a in full, "));
    assert_ne!(progress.next().unwrap(), progress.next().unwrap());
    assert!(!rig.stage(&later, "check.started").exists());
    fs::write(rig.stage(&first, "full.release"), []).unwrap();
    tty.expect(FIX_PROMPT, 1);
    tty.send("y\n");
    tty.expect(FIX_PROMPT, 2);
    tty.send("y\n");
    let (stdout, _) = tty.finish(1);
    assert!(stdout.contains("full checks are not supported"), "{stdout}");
    assert!(rig.stage(&first, "fix-full.finished").is_file());
    assert!(!rig.stage(&first, "fix.started").exists());
    assert!(rig.stage(&later, "fix.finished").is_file());
    assert!(!rig.stage(&later, "fix-full.started").exists());
}

#[test]
fn doctor_full_restores_after_success_and_failure() {
    for retry in ["clean", "crash"] {
        let mut rig = Rig::new();
        let program = rig.program(
            "blocked",
            json!({
                "full": "findings", "finding": "store.in_use", "full_retry": retry,
                "wait_stage": "full-retry",
            }),
        );
        let later = rig.program("later", json!({"full": "clean"}));
        let daemon = WireDaemon::start(
            &rig,
            Some(supervisor_row("blocked", "running", true)),
            StartReply::Ack,
        );
        let mut tty = Tty::start(&rig.command(&["doctor", "--full"]));
        tty.expect(DEEP_STOP_PROMPT, 1);
        assert!(daemon.enabled_calls().is_empty());
        tty.send("y\n");
        let pid = rig.pid(&program, "full-retry.started");
        assert_eq!(daemon.enabled_calls(), [false]);
        fs::write(rig.stage(&program, "full-retry.release"), []).unwrap();
        let (stdout, dialogue) = tty.finish(1);
        assert_eq!(daemon.enabled_calls(), [false, true]);
        assert_eq!(dialogue.matches(DEEP_STOP_PROMPT).count(), 1);
        assert!(!stdout.contains("store.in_use [blocked]"), "{stdout}");
        assert_eq!(
            stdout.contains("module_program.crashed [blocked]"),
            retry == "crash",
            "{stdout}"
        );
        assert!(rig.stage(&later, "full.started").exists());
        assert!(!rig.stage(&program, "check.started").exists());
        assert_gone(pid);
    }
}

#[test]
fn doctor_full_ctrl_c_cancels_the_stopped_check_and_restarts_promptly() {
    let mut rig = Rig::new();
    let program = rig.program(
        "blocked",
        json!({"full": "findings", "finding": "store.in_use", "full_retry": "sleep"}),
    );
    let later = rig.program("later", json!({"full": "clean"}));
    let daemon = WireDaemon::start(
        &rig,
        Some(supervisor_row("blocked", "running", true)),
        StartReply::Ack,
    );
    let report_path = rig.root.join("report.json");
    let mut tty = Tty::start(&rig.command(&["doctor", "--full", "--report", "report.json"]));
    tty.expect(DEEP_STOP_PROMPT, 1);
    tty.send("y\n");
    let pid = rig.pid(&program, "full-retry.started");
    assert_eq!(daemon.enabled_calls(), [false]);
    #[cfg(unix)]
    assert_eq!(tty.foreground(), Some(tty.pid().as_raw_pid() as u32));

    // The helper sleeps for a minute. The bound allows the runner's two-second
    // TERM-to-KILL grace period, but does not allow waiting for the helper to finish.
    let deadline = Instant::now() + Duration::from_secs(6);
    tty.send("\x03");
    while daemon.enabled_calls() != [false, true] && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let restarted_promptly = daemon.enabled_calls() == [false, true];
    #[cfg(unix)]
    if !restarted_promptly {
        // A build that doesn't cancel the check on Ctrl-C leaves ck running; end the
        // ck process this test started so the failure doesn't leave it behind.
        let _ = kill_process(Pid::from_raw(pid as i32).unwrap(), Signal::TERM);
    }
    let (stdout, dialogue) = tty.finish(130);
    assert!(
        restarted_promptly,
        "Ctrl-C did not restart the module within 6 s: {dialogue}"
    );
    assert_eq!(daemon.enabled_calls(), [false, true]);
    assert!(dialogue.contains("Cancelling the full check"), "{dialogue}");
    assert!(
        stdout.contains("blocked: full check interrupted."),
        "{stdout}"
    );
    assert_eq!(dialogue.matches(DEEP_STOP_PROMPT).count(), 1);
    let report = document(&report_path);
    assert_eq!(
        report["modules"][0],
        json!({
            "id": "blocked", "outcome": "interrupted", "mode": "full", "findings": 0, "detail": null,
        })
    );
    assert!(!report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["owner"] == "blocked" && row["layer"] == "module_program"));
    assert!(!rig.stage(&later, "full.started").exists());
    assert_gone(pid);
}

#[test]
fn doctor_full_decline_keeps_store_in_use() {
    let mut rig = Rig::new();
    let program = rig.program(
        "blocked",
        json!({"full": "findings", "finding": "store.in_use"}),
    );
    let daemon = WireDaemon::start(
        &rig,
        Some(supervisor_row("blocked", "running", true)),
        StartReply::Ack,
    );
    let mut tty = Tty::start(&rig.command(&["doctor", "--full"]));
    tty.expect(DEEP_STOP_PROMPT, 1);
    tty.send("n\n");
    let (stdout, dialogue) = tty.finish(1);
    assert_eq!(dialogue.matches(DEEP_STOP_PROMPT).count(), 1);
    assert!(stdout.contains("store.in_use [blocked]"), "{stdout}");
    assert!(!rig.stage(&program, "full-retry.started").exists());
    assert!(daemon.enabled_calls().is_empty());
}

#[test]
fn doctor_full_does_not_ask_again_when_rechecking_after_fix() {
    let mut rig = Rig::new();
    let program = rig.program(
        "blocked",
        json!({"full": "findings", "finding": "store.in_use"}),
    );
    let daemon = WireDaemon::start(
        &rig,
        Some(supervisor_row("blocked", "running", true)),
        StartReply::Ack,
    );
    let mut tty = Tty::start(&rig.command(&["doctor", "--full", "--fix"]));
    tty.expect(DEEP_STOP_PROMPT, 1);
    tty.send("n\n");
    tty.expect(FIX_PROMPT, 1);
    tty.send("y\n");
    let (stdout, dialogue) = tty.finish(1);
    assert_eq!(dialogue.matches(DEEP_STOP_PROMPT).count(), 1);
    assert_eq!(dialogue.matches("Checking blocked in full, 0 s").count(), 2);
    assert!(stdout.contains("store.in_use [blocked]"), "{stdout}");
    assert!(rig.stage(&program, "fix-full.finished").is_file());
    assert!(daemon.enabled_calls().is_empty());
}

#[test]
fn doctor_full_help_names_the_flag_and_limit() {
    let output = Rig::new().output(&["doctor", "--help"]);
    assert!(output.status.success());
    let stdout = text(&output.stdout);
    assert!(stdout.lines().any(|line| line
        == "  ck doctor [--json] [--report <file>] [--full] [--fix] [--subc <connection-file>]"));
    assert!(stdout.lines().any(|line| line
        == "  --full           run deep checks one module at a time (up to 30 minutes each)"));
}

#[test]
fn doctor_issue_ignores_a_daemon_only_module_finding() {
    let mut rig = Rig::new();
    let program = rig.program("blocked", json!({"check": "clean"}));
    let _daemon = WireDaemon::start(
        &rig,
        Some(supervisor_row("blocked", "failed", false)),
        StartReply::Ack,
    );
    let api = FakeApi::public();
    let tty = rig.issue_tty(&api);
    let (stdout, dialogue) = tty.finish(1);
    assert!(stdout.contains("module.failed [blocked]"), "{stdout}");
    assert!(
        stdout.ends_with("No module findings to file.\n"),
        "{stdout}"
    );
    assert!(!dialogue.contains(TITLE_PROMPT));
    assert!(!rig.stage(&program, "manifest.started").exists());
    api.assert_requests(0);
}

#[test]
fn doctor_sigint_to_ck_or_to_the_foreground_fix_restores_owned_stop() {
    // Windows gives the helper a separate process group and has no terminal
    // foreground group to transfer. Ctrl-C there goes to ck, not the helper.
    let foreground_cases = if cfg!(windows) {
        &[false][..]
    } else {
        &[false, true]
    };
    for &foreground in foreground_cases {
        let mut rig = Rig::new();
        let program = rig.program("stopped", json!({"fix": "sleep", "needs_stop": true}));
        let later = rig.program("z-later", json!({}));
        let daemon = WireDaemon::start(
            &rig,
            Some(supervisor_row("stopped", "running", true)),
            StartReply::Ack,
        );
        let mut tty = Tty::start(&rig.command(&["doctor", "--fix", "--json"]));
        tty.expect(FIX_PROMPT, 1);
        tty.send("y\n");
        let pid = rig.pid(&program, "fix.started");
        assert_eq!(daemon.enabled_calls(), [false]);
        if foreground {
            // The child owns the terminal so its own consent dialogue can work.
            #[cfg(unix)]
            assert_eq!(tty.foreground(), Some(pid));
            tty.send("\x03");
        } else {
            tty.interrupt();
            tty.expect("Interrupted; waiting for", 1);
            tty.interrupt();
            tty.expect("Interrupted; waiting for", 2);
        }
        let (stdout, dialogue) = tty.finish(130);
        let result: Value = serde_json::from_str(&stdout).unwrap();
        let fix = &result["fix"]["module_fixes"][0];
        assert_eq!(
            fix["result"]["outcome"],
            if foreground { "no_result" } else { "fixed" },
            "{fix}"
        );
        if foreground {
            assert_eq!(fix["result"]["code"], "crashed", "{fix}");
        }
        assert_eq!(fix["restart"], "enabled");
        assert_eq!(
            result["fix"]["module_fixes"][1]["skip_reason"],
            "interrupted"
        );
        assert_eq!(dialogue.matches(FIX_PROMPT).count(), 1);
        assert!(!rig.stage(&later, "fix.started").exists());
        assert_eq!(daemon.enabled_calls(), [false, true]);
        assert_gone(pid);
    }
}

#[test]
fn doctor_interrupt_during_stop_skips_fix_and_during_start_overrides_failure() {
    for start_reply in [StartReply::Ack, StartReply::Unanswered] {
        let mut rig = Rig::new();
        let program = rig.program("stopped", json!({"needs_stop": true}));
        let daemon = WireDaemon::start(
            &rig,
            Some(supervisor_row("stopped", "running", true)),
            start_reply,
        );
        let mut command = rig.command(&["doctor", "--fix", "--json"]);
        command.env("CK_TEST_OPERATOR_RESPONSE_BUDGET_MS", "100");
        let mut tty = Tty::start(&command);
        tty.expect(FIX_PROMPT, 1);
        let during_stop = matches!(start_reply, StartReply::Ack);
        if during_stop {
            daemon
                .events
                .hold_stop
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        tty.send("y\n");
        if during_stop {
            until(|| (daemon.enabled_calls() == [false]).then_some(()));
            tty.secondary_interrupt();
            tty.expect("waiting for a module stop or operator approval", 1);
            daemon.events.stop_ack.notify_one();
        } else {
            until(|| (daemon.enabled_calls() == [false, true]).then_some(()));
            tty.secondary_interrupt();
            tty.expect("Interrupted; waiting for", 1);
        }
        let (stdout, _) = tty.finish(SECONDARY_EXIT);
        let result: Value = serde_json::from_str(&stdout).unwrap();
        let fix = &result["fix"]["module_fixes"][0];
        assert_eq!(daemon.enabled_calls(), [false, true]);
        if during_stop {
            assert_eq!(fix["skip_reason"], "interrupted");
            assert_eq!(fix["restart"], "enabled");
            assert!(!rig.stage(&program, "fix.started").exists());
        } else {
            assert_eq!(fix["result"]["outcome"], "fixed");
            assert_eq!(fix["restart"], "failed");
            assert_gone(rig.pid(&program, "fix.finished"));
        }
    }
}

#[test]
fn doctor_interrupt_during_first_pass_does_not_revert_an_offered_window() {
    use ck_ui::time::{Timestamp, UtcOffset};
    use subc_core::window::journal::{Action, Owner, Record, Step};

    let mut rig = Rig::new();
    let program = rig.program("slow", json!({"check_sleep_ms": 1000}));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let time = Timestamp::from_unix_millis(now as i64).civil(UtcOffset::UTC);
    let id = format!(
        "{:04}{:02}{:02}-{}-1234",
        time.year,
        time.month,
        time.day,
        time.hms().replace(':', "")
    );
    let root = fs::canonicalize(rig.root.join("data/cortexkit")).unwrap();
    let directory = root.join("windows").join(&id);
    fs::create_dir_all(&directory).unwrap();
    let journal = directory.join("journal");
    {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;
        #[cfg(unix)]
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&journal)
            .unwrap();
        #[cfg(windows)]
        let mut file = subc_os::windows_acl::create_private_file(&journal).unwrap();
        let begin = Record::Begin {
            version: 1,
            id: id.clone(),
            began_ms: now,
            unprotected: true,
            note: None,
        };
        // Preserve the platform's boot identity, but represent a process that
        // no longer owns the window because its start time does not match.
        let mut owner = Owner::current().unwrap();
        owner.start_time = if owner.start_time == 1 { 2 } else { 1 };
        let step = Record::Step {
            step: Box::new(Step {
                target: root.join("test-fixture"),
                action: Action::File {
                    before: None,
                    after: Some("0".repeat(64)),
                    mode: None,
                },
                owner,
                module: None,
                role: None,
                config_generation: None,
            }),
        };
        for record in [begin, step, Record::Done { index: 1 }] {
            writeln!(file, "{}", serde_json::to_string(&record).unwrap()).unwrap();
        }
    }
    let before = fs::read(&journal).unwrap();
    let mut tty = Tty::start(&rig.command(&["doctor", "--fix", "--json"]));
    let pid = rig.pid(&program, "check.started");
    tty.secondary_interrupt();
    tty.expect("waiting for a module check", 1);
    let (stdout, dialogue) = tty.finish(SECONDARY_EXIT);
    let result: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(result["offer"]["window_id"], id, "{result}");
    assert_eq!(result["fix"]["skipped"], "interrupted", "{result}");
    assert_eq!(
        result["fix"]["module_fixes"][0]["skip_reason"],
        "interrupted"
    );
    assert!(!dialogue.contains(FIX_PROMPT));
    assert_eq!(fs::read(&journal).unwrap(), before);
    assert_gone(pid);
}
