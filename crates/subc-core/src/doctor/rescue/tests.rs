use std::{
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, SystemTime},
};

use cortexkit_test_support::ScratchDir;

use super::{
    auth::Login,
    host::{Child, ChildOutput},
    pins::{self, NodeArchive, NODE_VERSION, PI_PACKAGE, PI_VERSION},
    *,
};

mod filesystem;

const BIN: &str = "dist/bundle/cli.js";

struct Fake {
    _scratch: ScratchDir,
    root: PathBuf,
    daemon: Result<PathBuf, String>,
    platform: Platform,
    children: Mutex<Vec<Child>>,
    events: Mutex<Vec<String>>,
    probe: Result<String, &'static str>,
    good_hash: bool,
    good_node: bool,
    good_npm: bool,
    good_pi_version: bool,
    npm_code: i32,
    login_code: i32,
    launch_error: bool,
    ready: Option<&'static str>,
    login_makes_ready: bool,
    choice: Option<Login>,
    check_failure: Option<CheckFailure>,
    did_login: AtomicBool,
    archive_file: Option<PathBuf>,
    download_pause: Option<Arc<Pause>>,
    npm_pause: Option<Arc<Pause>>,
    launch_pause: Option<Arc<Pause>>,
    waiting: Option<Arc<Pause>>,
    check_install_locks: bool,
    fail_prune: bool,
}

#[derive(Clone, Copy)]
enum CheckFailure {
    Start,
    Signal,
    Exit(i32),
    Mismatch,
    Malformed,
    WrongProvider,
}

impl Fake {
    fn new() -> Self {
        let scratch = ScratchDir::new("doctor-rescue");
        let base = fs::canonicalize(scratch.path()).unwrap();
        Self {
            _scratch: scratch,
            root: base.join(INSTALL_DIR),
            daemon: Ok(base.join("data/cortexkit/run")),
            platform: Platform {
                os: "linux",
                arch: "x86_64",
            },
            children: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            probe: Err("could not start node"),
            good_hash: true,
            good_node: true,
            good_npm: true,
            good_pi_version: true,
            npm_code: 0,
            login_code: 0,
            launch_error: false,
            ready: Some("anthropic"),
            login_makes_ready: true,
            choice: None,
            check_failure: None,
            did_login: AtomicBool::new(false),
            archive_file: None,
            download_pause: None,
            npm_pause: None,
            launch_pause: None,
            waiting: None,
            check_install_locks: false,
            fail_prune: false,
        }
    }

    fn tools(&self, name: &str) -> PathBuf {
        self.root.join("tools").join(name)
    }
    fn open_managed(&self, names: &[&str], create: bool) -> Directory {
        let mut folder = Directory::root(&self.data_home().unwrap(), INSTALL_DIR, false)
            .unwrap()
            .unwrap();
        for name in names {
            folder = folder.managed(name, create).unwrap();
        }
        folder
    }
    fn profile(&self) -> PathBuf {
        self.root.join("rescue/pi-profile")
    }
    fn sessions(&self) -> PathBuf {
        self.root.join("rescue/sessions")
    }
    fn pi(&self) -> PathBuf {
        self.tools("pi").join(PI_VERSION)
    }
    fn node_root(&self) -> PathBuf {
        self.tools("node").join(NODE_VERSION)
    }
    fn event(&self, event: impl Into<String>) {
        self.events.lock().unwrap().push(event.into());
    }

    fn path_node(&mut self, version: &str, with_npm: bool) -> PathBuf {
        let executable = seed_node(
            &self.root.with_file_name("path-node"),
            self.platform,
            with_npm,
        );
        self.probe = Ok(format!("{}\n{version}\n", executable.display()));
        executable
    }

    fn seeded(mut self) -> Self {
        self.path_node("v22.19.0", false);
        seed_pi(&self.pi(), BIN, false);
        self
    }

    fn error(&self) -> String {
        prepare_with(self).err().expect("rescue should fail")
    }
}

impl Host for Fake {
    fn data_home(&self) -> Result<PathBuf, String> {
        Ok(self.root.parent().unwrap().to_owned())
    }
    fn daemon_run_dir(&self) -> Result<PathBuf, String> {
        self.daemon.clone()
    }
    fn platform(&self) -> Platform {
        self.platform
    }
    fn environment(&self) -> Vec<(OsString, OsString)> {
        [
            ("PATH", "/person/bin"),
            ("SUBC_TEST", "bad"),
            ("AFT_TEST", "bad"),
            ("CK_TEST", "bad"),
            ("CK_RESCUE_SESSION", "wrong"),
            ("NODE_OPTIONS", "bad"),
            ("PI_CODING_AGENT_DIR", "wrong"),
            ("NPM_CONFIG_CACHE", "wrong"),
            ("OTHER", "kept"),
            ("aFt_lower", "platform-dependent"),
        ]
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
        .collect()
    }
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_709_164_800)
    }
    fn run(&self, child: Child) -> Result<ChildOutput, String> {
        let args: Vec<_> = child
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let cwd = child.cwd.clone();
        self.children.lock().unwrap().push(child.clone());
        let output = |code, text: &str| {
            Ok(ChildOutput {
                code: Some(code),
                signal: None,
                stdout: text.as_bytes().into(),
            })
        };
        match args
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice()
        {
            ["-p", expr] => {
                assert_eq!(*expr, r"process.execPath+'\n'+process.version");
                self.event("PATH probe");
                output(0, self.probe.as_ref().map_err(|error| error.to_string())?)
            }
            ["--version"] => {
                self.event("private probe");
                if self.archive_file.is_some() {
                    return System.run(child);
                }
                output(
                    if self.good_node { 0 } else { 99 },
                    &format!("v{NODE_VERSION}\n"),
                )
            }
            [_, "ci", "--ignore-scripts", "--no-audit", "--no-fund"] => {
                self.event("npm ci");
                assert_eq!(fs::read(cwd.join("package.json")).unwrap(), pins::WRAPPER);
                assert_eq!(
                    fs::read(cwd.join("package-lock.json")).unwrap(),
                    pins::LOCKFILE
                );
                if let Some(pause) = &self.npm_pause {
                    pause.enter();
                }
                seed_pi(&cwd, BIN, false);
                output(self.npm_code, "ignored npm output")
            }
            [_, "--version"] => {
                self.event("pi version");
                output(
                    if self.good_pi_version { 0 } else { 7 },
                    &format!("pi v{PI_VERSION}\n"),
                )
            }
            [_, "auth", "check", "--provider", provider, "--json"] => {
                self.event(format!("check {provider}"));
                if self.check_install_locks {
                    for tool in ["node", "pi"] {
                        let path = self.tools(tool).join(".lock");
                        if path.exists() {
                            let lock = self
                                .open_managed(&["tools", tool], false)
                                .open_lock(".lock")
                                .unwrap();
                            lock.try_lock()
                                .expect("sign-in does not hold either install lock");
                        }
                    }
                }
                if let Some(failure) = self.check_failure {
                    return match failure {
                        CheckFailure::Start => Err("could not start: executable is missing".into()),
                        CheckFailure::Signal => Ok(ChildOutput {
                            code: None,
                            signal: Some(15),
                            stdout: b"secret noise".into(),
                        }),
                        CheckFailure::Exit(code) => output(
                            code,
                            &format!(
                                r#"{{"status":"invalid","provider":"{provider}","secret":"secret noise"}}"#
                            ),
                        ),
                        CheckFailure::Mismatch => output(
                            0,
                            &format!(r#"{{"status":"not_ready","provider":"{provider}"}}"#),
                        ),
                        CheckFailure::Malformed => output(0, "secret noise"),
                        CheckFailure::WrongProvider => {
                            output(0, r#"{"status":"ready","provider":"wrong"}"#)
                        }
                    };
                }
                let ready = self.ready == Some(provider)
                    || (self.login_makes_ready && self.did_login.load(Ordering::SeqCst));
                if !ready && *provider == "anthropic" {
                    return output(1, include_str!("pins/pi-provider-not-ready.json"));
                }
                let status = if ready { "ready" } else { "not_ready" };
                output(
                    i32::from(!ready),
                    &format!(r#"{{"status":"{status}","provider":"{provider}"}}"#),
                )
            }
            [_, "auth", "login", ..] => {
                self.event("login");
                self.did_login.store(true, Ordering::SeqCst);
                output(self.login_code, "")
            }
            [_, "--system-prompt", ..] => {
                self.event("launch");
                if let Some(pause) = &self.launch_pause {
                    pause.enter();
                }
                if self.launch_error {
                    Err("could not start: executable vanished".into())
                } else {
                    output(19, "")
                }
            }
            _ => panic!("unexpected child argv: {args:?}"),
        }
    }
    fn download(&self, url: &str, destination: &Path) -> Result<(), String> {
        self.event(format!("download {url}"));
        if let Some(pause) = &self.download_pause {
            pause.enter();
        }
        if let Some(path) = &self.archive_file {
            fs::copy(path, destination).unwrap();
        } else {
            fs::write(destination, b"synthetic archive").unwrap();
        }
        Ok(())
    }
    fn extract(&self, archive: &Path, staging: &Path) -> Result<(), String> {
        self.event("extract");
        if self.archive_file.is_some() {
            return System.extract(archive, staging);
        }
        seed_node(
            &staging.join(self.archive().unwrap().folder()),
            self.platform,
            self.good_npm,
        );
        Ok(())
    }
    fn login(&self) -> Result<Option<Login>, String> {
        self.event("select login");
        Ok(self.choice)
    }
    fn archive(&self) -> Option<NodeArchive> {
        let mut archive = pins::node_archive(self.platform)?;
        if self.good_hash {
            use sha2::{Digest, Sha256};
            let actual = self
                .archive_file
                .as_ref()
                .map(|path| crate::setup::upgrade_assets::sha256_file(path).unwrap())
                .unwrap_or_else(|| format!("{:x}", Sha256::digest(b"synthetic archive")));
            archive.sha256 = actual.into();
        }
        Some(archive)
    }
    fn waiting(&self) {
        self.event("waiting");
        if let Some(pause) = &self.waiting {
            pause.enter();
        }
    }
    fn warn(&self, message: &str) {
        self.event(format!("warning {message}"));
    }
    fn remove_version(&self, path: &Path) -> std::io::Result<()> {
        if self.fail_prune {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected version deletion failure",
            ));
        }
        files::remove_entry(path)
    }
}

#[derive(Default)]
struct Pause {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

impl Pause {
    fn enter(&self) {
        let mut state = self.state.lock().unwrap();
        state.0 = true;
        self.changed.notify_all();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(10), |state| !state.1)
            .unwrap();
        assert!(state.1, "test never resumed the paused child");
    }
    fn entered(&self) {
        let state = self.state.lock().unwrap();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(5), |state| !state.0)
            .unwrap();
        assert!(state.0, "test child never entered the pause");
    }
    fn resume(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }
}

fn seed_node(root: &Path, platform: Platform, npm: bool) -> PathBuf {
    let program = root.join(if platform.windows() {
        "node.exe"
    } else {
        "bin/node"
    });
    fs::create_dir_all(program.parent().unwrap()).unwrap();
    fs::write(&program, b"fake Node").unwrap();
    if npm {
        let script = root
            .join(if platform.windows() {
                "node_modules"
            } else {
                "lib/node_modules"
            })
            .join("npm/bin/npm-cli.js");
        fs::create_dir_all(script.parent().unwrap()).unwrap();
        fs::write(script, b"fake npm CLI").unwrap();
    }
    program
}

fn seed_pi(project: &Path, script: &str, string_bin: bool) {
    let package = project.join("node_modules").join(PI_PACKAGE);
    let bin = if string_bin {
        serde_json::json!(script)
    } else {
        serde_json::json!({"pi": script})
    };
    let path = package.join(script);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, b"fake Pi script").unwrap();
    fs::write(
        package.join("package.json"),
        serde_json::to_vec(&serde_json::json!({"bin": bin})).unwrap(),
    )
    .unwrap();
}

fn no_staging(folder: &Path) {
    assert!(fs::read_dir(folder).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".staging-")));
}

#[test]
fn pin_and_real_pi_fixtures_agree() {
    let wrapper: serde_json::Value = serde_json::from_slice(pins::WRAPPER).unwrap();
    let lock: serde_json::Value = serde_json::from_slice(pins::LOCKFILE).unwrap();
    assert_eq!(wrapper["private"], true);
    assert_eq!(wrapper["dependencies"][PI_PACKAGE], PI_VERSION);
    assert_eq!(wrapper["dependencies"].as_object().unwrap().len(), 1);
    assert_eq!(lock["packages"][""]["dependencies"][PI_PACKAGE], PI_VERSION);
    assert_eq!(
        lock["packages"][format!("node_modules/{PI_PACKAGE}")]["version"],
        PI_VERSION
    );
    for (name, entry) in lock["packages"].as_object().unwrap() {
        if !name.is_empty() {
            assert!(
                entry["integrity"].as_str().unwrap().starts_with("sha"),
                "{name}"
            );
        }
    }
    assert!(tools::pi_version(include_bytes!("pins/pi-version.txt")));
    assert!(tools::pi_version(b"pi v1.6.0\n"));
    for wrong in ["1.6.01", "vv1.6.0", "1.6.0-stable", "1.6.00"] {
        assert!(!tools::pi_version(wrong.as_bytes()), "{wrong}");
    }
    let help = include_str!("pins/pi-login-help.txt");
    assert!(help.contains("pi auth login <provider> [--api-key]"));
    let providers: Vec<_> = include_str!("pins/pi-invalid-login.stderr")
        .lines()
        .find_map(|line| line.strip_prefix("Providers: "))
        .unwrap()
        .split(", ")
        .collect();
    for number in 1..=5 {
        assert!(providers.contains(&Login::choose(number).unwrap().provider()));
    }
    assert!(include_str!("pins/pi-bare-auth-check.stderr")
        .contains("Auth checks require --provider <provider> or --model <model>"));
    let clean: serde_json::Value =
        serde_json::from_str(include_str!("pins/pi-provider-not-ready.json")).unwrap();
    assert_eq!(clean["status"], "not_ready");
    assert_eq!(clean["provider"], "anthropic");
}

#[test]
fn fresh_install_and_launch_have_exact_arguments_and_clean_environment() {
    for platform in [
        Platform {
            os: "linux",
            arch: "x86_64",
        },
        Platform {
            os: "windows",
            arch: "aarch64",
        },
    ] {
        let fake = Fake {
            platform,
            ready: Some("openai"),
            check_install_locks: true,
            ..Fake::new()
        };
        let run = prepare_with(&fake).unwrap();
        let session = run.session_dir().to_owned();
        assert_eq!(session, fake.sessions().join("20240229T000000Z"));
        assert_eq!(run.install_root(), fake.root);
        assert_eq!(run.daemon_run_dir(), fake.daemon.as_deref().unwrap());
        run.write_session_file("report.json", b"one").unwrap();
        assert_eq!(
            run.write_session_file("report.json", b"two")
                .unwrap_err()
                .exit_code(),
            1
        );
        assert_eq!(fs::read(session.join("report.json")).unwrap(), b"one");
        assert!(run
            .write_session_file("../report.json", b"outside")
            .is_err());
        run.write_session_file(PROMPT, b"prompt").unwrap();
        run.launch_with(&fake).unwrap();

        let events = fake.events.lock().unwrap();
        assert_eq!(events[0], "PATH probe");
        assert!(events[1].starts_with("download https://nodejs.org/dist/"));
        assert_eq!(
            &events[2..],
            &[
                "extract",
                "private probe",
                "npm ci",
                "pi version",
                "check anthropic",
                "check openai-codex",
                "check openai",
                "launch"
            ]
        );
        no_staging(&fake.tools("pi"));
        no_staging(&fake.tools("node"));

        let children = fake.children.lock().unwrap();
        let node = fake.node_root().join(if platform.windows() {
            "node.exe"
        } else {
            "bin/node"
        });
        for child in children.iter() {
            let get = |key| {
                child
                    .env
                    .iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value.as_os_str())
            };
            for (key, _) in &child.env {
                let name = key.to_string_lossy();
                let name = if platform.windows() {
                    name.to_ascii_uppercase().into()
                } else {
                    name
                };
                assert!(
                    !name.starts_with("SUBC_")
                        && !name.starts_with("AFT_")
                        && name != "NODE_OPTIONS"
                );
                assert!(!name.starts_with("CK_") || name == "CK_RESCUE_SESSION");
            }
            assert_eq!(get("OTHER"), Some(OsStr::new("kept")));
            assert_eq!(get("CK_RESCUE_SESSION"), Some(OsStr::new("1")));
            assert_eq!(get("PI_CODING_AGENT_DIR"), Some(fake.profile().as_os_str()));
            if child.args[0] == "-p" {
                assert_eq!(child.program, Path::new("node"));
                assert_eq!(child.cwd, fake.tools(""));
                assert_eq!(child.timeout, Some(Duration::from_secs(2)));
            } else {
                let expected = if child.args == ["--version"] {
                    child.program.parent().unwrap()
                } else {
                    node.parent().unwrap()
                };
                assert_eq!(
                    std::env::split_paths(get("PATH").unwrap()).next().unwrap(),
                    expected
                );
                if child.args == ["--version"] {
                    assert_eq!(child.cwd, fake.tools(""));
                }
            }
            if child.args.get(1).is_some_and(|arg| arg == "ci") {
                assert_eq!(child.program, node);
                assert!(child.args[0].to_string_lossy().ends_with("npm-cli.js"));
                assert_eq!(child.cwd.parent().unwrap(), fake.tools("pi"));
                assert!(child
                    .cwd
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".staging-"));
                assert_eq!(
                    get("npm_config_cache"),
                    Some(fake.tools("npm-cache").as_os_str())
                );
                assert_eq!(
                    get("npm_config_logs_dir"),
                    Some(fake.tools("npm-cache/_logs").as_os_str())
                );
                assert_eq!(get("npm_config_update_notifier"), Some(OsStr::new("false")));
                assert!(get("NPM_CONFIG_CACHE").is_none());
            } else if child
                .args
                .get(1)
                .is_some_and(|arg| arg == "--version" || arg == "auth")
            {
                assert_eq!(child.cwd, fake.profile());
            }
        }
        let launch = children.last().unwrap();
        assert_eq!(launch.cwd, session);
        assert_eq!(launch.io, Io::Terminal);
        assert_eq!(launch.program, node);
        let script = fake.pi().join("node_modules").join(PI_PACKAGE).join(BIN);
        let prompt = session.join(PROMPT);
        assert_eq!(
            launch.args,
            [
                script.as_os_str(),
                OsStr::new("--system-prompt"),
                prompt.as_os_str(),
                OsStr::new("--no-extensions"),
                OsStr::new("--no-context-files"),
                OsStr::new("--no-skills"),
                OsStr::new("--provider"),
                OsStr::new("openai")
            ]
        );
    }
}

#[test]
fn new_enough_path_node_only_needs_npm_to_install_pi() {
    for (pi_exists, path_npm) in [(true, false), (false, true), (false, false)] {
        let mut fake = Fake::new();
        let path = fake.path_node("v22.19.0", path_npm);
        if pi_exists {
            seed_pi(&fake.pi(), BIN, true);
        }
        prepare_with(&fake).unwrap().launch_with(&fake).unwrap();
        let launch = &fake
            .children
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .program
            .clone();
        let node = if pi_exists || path_npm {
            path
        } else {
            fake.node_root().join("bin/node")
        };
        assert_eq!(launch, &node);
        assert_eq!(fake.node_root().exists(), !pi_exists && !path_npm);
    }
}

#[test]
fn malformed_old_and_slow_path_probes_fall_back_to_private_node() {
    for probe in [
        Ok("/usr/bin/node\nv22.18.99\n"),
        Ok("/node\nv22.19.0\nextra\n"),
        Ok("relative\nv25.0.0\n"),
        Ok("/node\n22.19.0\n"),
        Err("timed out after 2000 ms"),
    ] {
        let fake = Fake {
            probe: probe.map(str::to_owned),
            ..Fake::new()
        };
        prepare_with(&fake).unwrap();
        assert!(fake.node_root().is_dir());
    }
}

#[test]
fn private_node_without_npm_runs_existing_pi_but_is_replaced_to_reinstall() {
    let fake = Fake::new();
    seed_node(&fake.node_root(), fake.platform, false);
    seed_pi(&fake.pi(), BIN, false);
    prepare_with(&fake).unwrap().launch_with(&fake).unwrap();
    assert!(!fake.tools("node/.lock").exists());
    assert!(!fake.tools("pi/.lock").exists());
    assert!(!fake.node_root().join("lib").exists());
    fs::remove_dir_all(fake.pi()).unwrap();
    prepare_with(&fake).unwrap().launch_with(&fake).unwrap();
    assert!(fake
        .node_root()
        .join("lib/node_modules/npm/bin/npm-cli.js")
        .is_file());
    let events = fake.events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.starts_with("download "))
            .count(),
        1
    );
    assert_eq!(events.iter().filter(|event| *event == "npm ci").count(), 1);
}

#[test]
fn all_targets_have_distinct_archives_and_unknown_target_does_not_download() {
    for (os, arch, target, ext) in [
        ("macos", "x86_64", "darwin-x64", "tar.gz"),
        ("macos", "aarch64", "darwin-arm64", "tar.gz"),
        ("linux", "x86_64", "linux-x64", "tar.gz"),
        ("linux", "aarch64", "linux-arm64", "tar.gz"),
        ("windows", "x86_64", "win-x64", "zip"),
        ("windows", "aarch64", "win-arm64", "zip"),
    ] {
        let entry = pins::node_archive(Platform { os, arch }).unwrap();
        assert_eq!(entry.folder(), format!("node-v{NODE_VERSION}-{target}"));
        assert_eq!(
            entry.filename(),
            format!("node-v{NODE_VERSION}-{target}.{ext}")
        );
        assert_eq!(entry.sha256.len(), 64);
    }
    let fake = Fake {
        platform: Platform {
            os: "linux",
            arch: "mips",
        },
        ..Fake::new()
    };
    assert!(fake.error().contains("linux mips"));
    assert!(!fake
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|event| event.starts_with("download ")));
}

#[test]
fn bad_node_hash_or_payload_never_publishes_or_prunes() {
    for (good_hash, good_node, good_npm) in [
        (false, true, true),
        (true, false, true),
        (true, true, false),
    ] {
        let fake = Fake {
            good_hash,
            good_node,
            good_npm,
            ..Fake::new()
        };
        for version in ["0.1.0", "0.2.0"] {
            fs::create_dir_all(fake.tools("node").join(version)).unwrap();
        }
        let error = fake.error();
        let events = fake.events.lock().unwrap();
        assert_eq!(events.iter().any(|event| event == "extract"), good_hash);
        if good_hash {
            assert!(error.contains("npm-cli.js"));
        } else {
            assert!(
                error.contains("sha256 mismatch")
                    && error.contains("expected")
                    && error.contains("got")
            );
        }
        assert!(fake.tools("node/0.1.0").is_dir());
        assert!(!fake.node_root().exists());
        no_staging(&fake.tools("node"));
    }
}

#[test]
fn failed_npm_or_pi_check_keeps_old_pi_and_published_node() {
    for (npm_code, good_pi_version) in [(19, true), (0, false)] {
        let mut fake = Fake {
            npm_code,
            good_pi_version,
            ..Fake::new()
        };
        for version in ["0.1.0", "0.2.0"] {
            fs::create_dir_all(fake.tools("pi").join(version)).unwrap();
        }
        assert!(fake.error().contains(if npm_code != 0 {
            "npm ci: exit code 19"
        } else {
            "staged Pi did not report"
        }));
        assert!(fake.tools("pi/0.1.0").is_dir());
        assert!(fake.node_root().is_dir());
        assert!(!fake.pi().exists());
        no_staging(&fake.tools("pi"));
        fake.npm_code = 0;
        fake.good_pi_version = true;
        prepare_with(&fake).unwrap();
        assert!(fake.pi().is_dir());
        assert_eq!(
            fake.events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.starts_with("download "))
                .count(),
            1
        );
    }
}

#[test]
fn all_five_login_choices_have_exact_arguments_and_recheck_only_selected_provider() {
    for (number, provider, api_key) in [
        (1, "anthropic", false),
        (2, "anthropic", true),
        (3, "openai-codex", false),
        (4, "openai", true),
        (5, "github-copilot", false),
    ] {
        let fake = Fake {
            ready: None,
            choice: Login::choose(number),
            ..Fake::new().seeded()
        };
        prepare_with(&fake).unwrap().launch_with(&fake).unwrap();
        assert_eq!(
            &fake.events.lock().unwrap()[2..],
            &[
                "check anthropic",
                "check openai-codex",
                "check openai",
                "check github-copilot",
                "select login",
                "login",
                &format!("check {provider}"),
                "launch"
            ]
        );
        let children = fake.children.lock().unwrap();
        let login = children
            .iter()
            .find(|child| child.args.get(2).is_some_and(|arg| arg == "login"))
            .unwrap();
        assert_eq!(login.io, Io::Terminal);
        assert_eq!(login.cwd, fake.profile());
        let mut expected = vec!["auth", "login", provider];
        if api_key {
            expected.push("--api-key");
        }
        assert_eq!(&login.args[1..], expected);
        assert_eq!(children.last().unwrap().args.last().unwrap(), provider);
    }
}

#[test]
fn auth_failures_name_provider_and_status_without_leaking_child_output() {
    for (failure, expected) in [
        (
            CheckFailure::Start,
            "could not start: executable is missing",
        ),
        (CheckFailure::Signal, "signal 15"),
        (CheckFailure::Exit(23), "exit code 23"),
        (CheckFailure::Exit(2), "invalid (exit code 2)"),
        (CheckFailure::Mismatch, "invalid or mismatched result"),
        (CheckFailure::Malformed, "invalid or mismatched result"),
        (CheckFailure::WrongProvider, "invalid or mismatched result"),
    ] {
        let fake = Fake {
            check_failure: Some(failure),
            ..Fake::new().seeded()
        };
        let error = fake.error();
        assert!(
            error.contains("anthropic") && error.contains(expected),
            "{error}"
        );
        assert!(!error.contains("secret noise"));
        assert!(!fake.sessions().exists());
    }
}

#[test]
fn cancelled_login_failed_login_and_not_ready_recheck_leave_no_session() {
    for (choice, login_code, expected) in [
        (None, 0, "cancelled"),
        (Some(Login::Copilot), 9, "exit code 9"),
        (Some(Login::Copilot), 0, "not ready after sign-in"),
    ] {
        let fake = Fake {
            ready: None,
            choice,
            login_code,
            login_makes_ready: false,
            ..Fake::new().seeded()
        };
        assert!(fake.error().contains(expected));
        assert!(!fake.sessions().exists());
        assert!(!fake
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event == "launch"));
    }
}

#[test]
fn invalid_roots_write_nothing_and_forget_never_resolves_daemon_home() {
    for os in ["linux", "windows"] {
        let mut fake = Fake {
            platform: Platform { os, arch: "x86_64" },
            ..Fake::new()
        };
        let absolute = fake.root.clone();
        fake.root = "relative".into();
        assert!(fake.error().contains("must be absolute"));
        assert!(forget_with(&fake)
            .err()
            .unwrap()
            .contains("must be absolute"));
        fake.root = absolute;
        fake.daemon = Err("relative daemon data home".into());
        assert_eq!(fake.error(), "relative daemon data home");
        let absent = forget_with(&fake).unwrap();
        assert_eq!(absent.path, fake.profile().join(AUTH));
        assert!(!absent.removed);
        assert!(!fake.root.exists());
        assert!(fake.events.lock().unwrap().is_empty());
    }
}

#[test]
fn launch_start_error_is_reported() {
    let fake = Fake {
        launch_error: true,
        ..Fake::new().seeded()
    };
    let error = prepare_with(&fake).unwrap().launch_with(&fake).unwrap_err();
    assert_eq!(error, "Pi launch: could not start: executable vanished");
}
