#[cfg(windows)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{io::Read, thread, time::Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use super::{assert_exit, common, fs, text, Command, DoctorRig, Duration, Path, Value};

const CK: &str = env!("CARGO_BIN_EXE_ck-under-test");
#[cfg(windows)]
const FIXTURE: &str = env!("CARGO_BIN_EXE_doctor-module-fixture");
const LOG_MARKER: &str = "saved before shutdown";

fn comparable_path(path: &str) -> String {
    #[cfg(windows)]
    return path
        .strip_prefix(r"\\?\")
        .unwrap_or(path)
        .replace('\\', "/");
    #[cfg(unix)]
    path.to_owned()
}

struct RescueRig(DoctorRig);

impl RescueRig {
    fn new() -> Self {
        Self(DoctorRig::new())
    }

    fn command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        command.env_clear().current_dir(self.0.root.path());
        common::kit::isolate_client_fully(&mut command, self.0.root.path());
        #[cfg(windows)]
        for name in ["SystemRoot", "WINDIR", "COMSPEC"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .env("PATH", self.0.root.join("fake-bin"))
            .env("TERM", "xterm-256color")
            .env("CK_NODE_DIST_URL", "http://127.0.0.1:0/");
        command
    }

    fn install_root(&self) -> std::path::PathBuf {
        self.0.root.join("data/cortexkit")
    }

    fn seed_offline(&self) {
        self.0.write_offline_module();
        let bin = self.0.root.join("fake-bin");
        fs::create_dir(&bin).unwrap();
        let node = bin.join(format!("node{}", std::env::consts::EXE_SUFFIX));
        #[cfg(unix)]
        fs::write(
            &node,
            r#"#!/bin/sh
case "$1" in
  -p) printf '%s\nv22.19.0\n' "$0" ;;
  --version) echo v22.19.0 ;;
  *) exec /bin/sh "$@" ;;
esac
"#,
        )
        .unwrap();
        #[cfg(unix)]
        fs::set_permissions(&node, fs::Permissions::from_mode(0o700)).unwrap();
        #[cfg(windows)]
        fs::copy(FIXTURE, &node).unwrap();
        let package = self
            .install_root()
            .join("tools/pi/1.6.0/node_modules/@earendil-works/pi-coding-agent");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("package.json"), r#"{"bin":{"pi":"cli.js"}}"#).unwrap();
        fs::write(
            package.join("cli.js"),
            r#"case "$*" in
  --version) echo 'v1.6.0' ;;
  'auth check --provider anthropic --json') echo '{"status":"ready","provider":"anthropic"}' ;;
  --system-prompt*)
    printf '%s\n' "$0" "$@" > launch-argv
    /usr/bin/env > launch-env
    pwd > launch-cwd
    exit 23
    ;;
  *) exit 99 ;;
esac
"#,
        )
        .unwrap();
    }

    fn ckni(&self) -> std::path::PathBuf {
        let path = self
            .0
            .root
            .join(format!("ckni{}", std::env::consts::EXE_SUFFIX));
        fs::hard_link(CK, &path)
            .or_else(|_| fs::copy(CK, &path).map(|_| ()))
            .unwrap();
        path
    }
}

fn run_terminal(command: &Command) -> (u32, String) {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut builder = CommandBuilder::new(command.get_program());
    builder.args(command.get_args());
    builder.cwd(command.get_current_dir().unwrap());
    builder.env_clear();
    for (name, value) in command.get_envs() {
        if let Some(value) = value {
            builder.env(name, value);
        }
    }
    let mut reader = pair.master.try_clone_reader().unwrap();
    #[cfg(windows)]
    let mut writer = pair.master.take_writer().unwrap();
    let mut child = pair.slave.spawn_command(builder).unwrap();
    drop(pair.slave);
    let read = thread::spawn(move || {
        let mut bytes = Vec::new();
        #[cfg(unix)]
        // Some Unix pseudo-terminals (PTYs) return EIO rather than EOF when the slave closes.
        if let Err(error) = reader.read_to_end(&mut bytes) {
            assert_eq!(
                error.raw_os_error(),
                Some(rustix::io::Errno::IO.raw_os_error())
            );
        }
        #[cfg(windows)]
        {
            let mut answered = false;
            let mut chunk = [0; 4096];
            loop {
                let n = reader.read(&mut chunk).unwrap();
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..n]);
                if !answered && bytes.windows(4).any(|bytes| bytes == b"\x1b[6n") {
                    // Answer the Windows pseudo-terminal's cursor query so output can start.
                    answered = true;
                    writer.write_all(b"\x1b[1;1R").unwrap();
                    writer.flush().unwrap();
                }
            }
        }
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("rescue terminal command did not exit within 15 seconds");
        }
        thread::sleep(Duration::from_millis(10));
    };
    drop(pair.master);
    (
        status.exit_code(),
        String::from_utf8(read.join().unwrap()).unwrap(),
    )
}

#[test]
fn rescue_runs_offline_on_a_terminal_but_not_under_ckni() {
    let rig = RescueRig::new();
    rig.seed_offline();
    let ckni = rig.ckni();
    let (code, terminal) = run_terminal(rig.command(&ckni).args(["doctor", "--agent"]));
    assert_eq!(code, 2, "{terminal}");
    assert!(
        terminal.contains("doctor --agent cannot run under ckni"),
        "{terminal}"
    );
    assert!(!rig.install_root().join("rescue").exists());

    let (code, terminal) = run_terminal(rig.command(Path::new(CK)).args(["doctor", "--agent"]));
    assert_eq!(code, 0, "{terminal}");
    assert!(
        terminal.contains("can read report.json and send its contents"),
        "{terminal}"
    );
    let sessions = rig.install_root().join("rescue/sessions");
    let mut entries = fs::read_dir(&sessions).unwrap();
    let session = entries.next().unwrap().unwrap().path();
    assert!(entries.next().is_none());
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&session).unwrap().permissions().mode() & 0o777,
        0o700
    );
    #[cfg(windows)]
    subc_os::windows_acl::verify_private_dir(&session).unwrap();

    let report_path = session.join("report.json");
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(&report_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    #[cfg(windows)]
    subc_os::windows_acl::test_support::assert_owner_only(&report_path, false, true);
    let report: Value = serde_json::from_slice(&fs::read(&report_path).unwrap()).unwrap();
    assert_eq!(report["schema"], "ck.doctor.report/v1");
    assert_eq!(report["daemon_answered"], false);
    assert_eq!(report["report"]["sanitized"], false);
    assert_eq!(
        fs::canonicalize(report["report"]["path"].as_str().unwrap()).unwrap(),
        fs::canonicalize(&report_path).unwrap()
    );
    assert!(
        report["log_tails"].as_array().unwrap().iter().any(|tail| {
            tail["entries"].as_array().unwrap().iter().any(|entry| {
                entry["text"]
                    .as_str()
                    .is_some_and(|line| line.contains(LOG_MARKER))
            })
        }),
        "{report}"
    );

    let prompt_path = session.join("rescue-prompt.md");
    let prompt = fs::read_to_string(&prompt_path).unwrap();
    #[cfg(unix)]
    let shell_word = format!(
        "'{}'",
        fs::canonicalize(CK)
            .unwrap()
            .to_str()
            .unwrap()
            .replace('\'', "'\\''")
    );
    #[cfg(windows)]
    let shell_word = {
        let word = prompt
            .lines()
            .find_map(|line| {
                line.strip_prefix("- `")?
                    .split_once(" doctor`")
                    .map(|(word, _)| word)
            })
            .expect("the rescue prompt must show the doctor command");
        let path = word
            .strip_prefix('\'')
            .and_then(|word| word.strip_suffix('\''))
            .unwrap()
            .replace("'\\''", "'")
            .replace('/', "\\");
        assert!(Path::new(&path).is_absolute());
        // The test runner may map a Windows drive letter to a directory. Compare
        // file IDs because the drive alias and backing path can name the same ck.
        let identity =
            |path| subc_os::retained_file_identity(&fs::File::open(path).unwrap()).unwrap();
        assert_eq!(identity(path.as_str()), identity(CK));
        word
    };
    for command in [
        "doctor",
        "stack status",
        "stack stop",
        "stack start",
        "window list",
        "window revert",
        "module list",
        "module logs MODULE_ID",
        "module restart MODULE_ID",
    ] {
        let command = format!("`{shell_word} {command}`");
        assert!(prompt.contains(&command), "missing {command}");
    }
    for (_, id) in subc_core::setup::supervised_module_ids() {
        assert!(prompt.contains(&format!("`{id}`")), "missing {id}");
    }
    for element in [
        "ck-mcp-stdio-adapter",
        "ck-bus",
        "report.json",
        "untrusted diagnostic data",
        "Never delete a store",
        "Never edit or delete anything under a module's data folder",
        "Never change anything without a backup",
        "Never edit files under the config folder",
        "Never run `ck doctor --agent`",
        "Daemon answered: no",
        "`module.daemon_unavailable`",
    ] {
        assert!(prompt.contains(element), "missing {element}");
    }
    for path in [
        rig.install_root(),
        rig.0.root.join("data"),
        rig.0.root.join("config/cortexkit/subc.jsonc"),
        rig.0.root.join("data/cortexkit/run/logs"),
    ] {
        #[cfg(unix)]
        assert!(
            prompt.contains(&format!("`{}`", path.display())),
            "missing {path:?}"
        );
        #[cfg(windows)]
        assert!(
            prompt
                .replace(r"\\?\", "")
                .replace('\\', "/")
                .contains(&format!(
                    "`{}`",
                    path.display().to_string().replace('\\', "/")
                )),
            "missing {path:?}"
        );
    }
    assert!(!prompt.contains(LOG_MARKER));
    assert!(!prompt.contains("{{"));

    let argv = fs::read_to_string(session.join("launch-argv")).unwrap();
    let args = argv.lines().collect::<Vec<_>>();
    let package = rig
        .install_root()
        .join("tools/pi/1.6.0/node_modules/@earendil-works/pi-coding-agent");
    assert_eq!(
        fs::canonicalize(args[0]).unwrap(),
        fs::canonicalize(package.join("cli.js")).unwrap()
    );
    assert_eq!(args[1], "--system-prompt");
    assert_eq!(
        fs::canonicalize(args[2]).unwrap(),
        fs::canonicalize(&prompt_path).unwrap()
    );
    assert_eq!(
        args[3..],
        [
            "--no-extensions",
            "--no-context-files",
            "--no-skills",
            "--provider",
            "anthropic",
        ]
    );
    let cwd = fs::read_to_string(session.join("launch-cwd")).unwrap();
    assert_eq!(
        fs::canonicalize(cwd.trim()).unwrap(),
        fs::canonicalize(&session).unwrap()
    );
    let env = fs::read_to_string(session.join("launch-env")).unwrap();
    let profile = env
        .lines()
        .find_map(|line| line.strip_prefix("PI_CODING_AGENT_DIR="))
        .unwrap();
    assert_eq!(
        fs::canonicalize(profile).unwrap(),
        fs::canonicalize(rig.install_root().join("rescue/pi-profile")).unwrap()
    );
    let control_env: Vec<_> = env
        .lines()
        .filter(|line| {
            ["CK_", "SUBC_", "AFT_", "NODE_OPTIONS="]
                .iter()
                .any(|prefix| line.starts_with(prefix))
        })
        .collect();
    assert_eq!(control_env, ["CK_RESCUE_SESSION=1"]);

    let protected = rig.install_root().join("rescue/x.json");
    let output = rig
        .command(Path::new(CK))
        .args(["doctor", "--report"])
        .arg(&protected)
        .output()
        .unwrap();
    assert_exit(&output, 2);
    assert!(!protected.exists());
}

#[test]
fn rescue_prompt_refuses_backticks_and_line_breaks_before_launch() {
    for bad_config in ["config`bad", "config\nbad"] {
        let rig = RescueRig::new();
        rig.seed_offline();
        let (code, terminal) = run_terminal(
            rig.command(Path::new(CK))
                .env("XDG_CONFIG_HOME", rig.0.root.join(bad_config))
                .args(["doctor", "--agent"]),
        );
        assert_eq!(code, 1, "{terminal}");
        assert!(terminal.contains("line break or backtick"), "{terminal}");
        let session = fs::read_dir(rig.install_root().join("rescue/sessions"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(!session.join("launch-argv").exists());
        assert!(!session.join("rescue-prompt.md").exists());
    }
}

#[test]
fn rescue_help_and_parser_refuse_before_a_rescue_session_check() {
    let rig = RescueRig::new();
    let run = |args: &[&str]| {
        rig.command(Path::new(CK))
            .env("CK_RESCUE_SESSION", "")
            .args(args)
            .output()
            .unwrap()
    };
    let help = run(&["doctor", "--help"]);
    assert_exit(&help, 0);
    assert!(text(&help.stdout).contains("ck doctor --agent [--forget]"));

    for repeated in ["--agent", "--forget"] {
        let output = run(&["--json", "doctor", repeated, repeated]);
        assert_exit(&output, 2);
        assert!(text(&output.stderr)
            .contains(&format!("unknown or repeated doctor argument {repeated:?}")));
    }
    for mut args in [
        vec!["--json", "doctor", "--agent"],
        vec!["doctor", "--agent", "--json"],
    ] {
        for forget in [false, true] {
            if forget {
                args.push("--forget");
            }
            let output = run(&args);
            assert_exit(&output, 2);
            assert!(text(&output.stderr).contains("doctor --agent has no JSON form"));
            assert!(output.stdout.is_empty());
        }
    }
    assert!(!rig.install_root().exists());
}

#[test]
fn rescue_forget_is_local_without_a_terminal_under_ckni() {
    let rig = RescueRig::new();
    let ckni = rig.ckni();
    let profile = rig.install_root().join("rescue/pi-profile");
    let auth = profile.join("auth.json");
    let forget = || {
        let mut command = rig.command(&ckni);
        command
            .env("SUBC_CONNECTION_FILE", "not-the-local-stack.json")
            .args([
                "doctor",
                "--agent",
                "--forget",
                "--subc",
                "other-stack.json",
            ]);
        command
    };

    let output = forget().output().unwrap();
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    let found = stdout.trim().strip_prefix("Not found: ").unwrap();
    assert_eq!(
        comparable_path(found),
        comparable_path(auth.to_str().unwrap())
    );
    assert!(!rig.install_root().exists());

    fs::create_dir_all(&profile).unwrap();
    for dir in [&profile, profile.parent().unwrap()] {
        #[cfg(unix)]
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
        #[cfg(windows)]
        subc_os::windows_acl::create_private_dir(dir).unwrap();
    }
    fs::write(&auth, "{}").unwrap();
    let transcript = profile.join("transcript.jsonl");
    fs::write(&transcript, "unchanged").unwrap();
    let recursive = forget().env("CK_RESCUE_SESSION", "1").output().unwrap();
    assert_exit(&recursive, 2);
    assert!(text(&recursive.stderr).contains("CK_RESCUE_SESSION is set"));
    assert!(auth.is_file());
    let output = forget().output().unwrap();
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    let deleted = stdout.trim().strip_prefix("Deleted ").unwrap();
    assert_eq!(
        comparable_path(deleted),
        comparable_path(auth.to_str().unwrap())
    );
    assert!(!auth.exists());
    assert_eq!(fs::read_to_string(transcript).unwrap(), "unchanged");
}
