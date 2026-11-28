#![forbid(unsafe_code)]

use std::{
    env, fs,
    io::{self, Write},
    path::PathBuf,
    process, thread,
    time::{Duration, Instant},
};

use cortexkit_doctor::{
    CheckOutput, Evidence, EvidenceValue, Finding, FindingId, Fix, FixOutput, LogSource, Manifest,
    ModuleId, PathCategory, Severity, Template, MAX_OUTPUT_BYTES,
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(default)]
struct Scenario {
    check: String,
    full: Option<String>,
    full_retry: Option<String>,
    wait_stage: Option<String>,
    fix: String,
    manifest: String,
    needs_stop: bool,
    severity: Severity,
    finding: String,
    risky: bool,
    repository: String,
    check_sleep_ms: u64,
    fix_sleep_ms: u64,
    clean_after_fix: bool,
    record_fix: bool,
    private_ref: Option<String>,
    private_path: Option<String>,
    record_stages: bool,
    manifest_sleep_ms: u64,
    log_template: Option<String>,
}

impl Default for Scenario {
    fn default() -> Self {
        Self {
            check: "findings".into(),
            full: None,
            full_retry: None,
            wait_stage: None,
            fix: "fixed".into(),
            manifest: "valid".into(),
            needs_stop: false,
            severity: Severity::Warning,
            finding: "module.failed".into(),
            risky: false,
            repository: "https://github.com/example/doctor-fixture".into(),
            check_sleep_ms: 0,
            fix_sleep_ms: 0,
            clean_after_fix: false,
            record_fix: false,
            private_ref: None,
            private_path: None,
            record_stages: false,
            manifest_sleep_ms: 0,
            log_template: None,
        }
    }
}

fn main() {
    let executable = env::current_exe().unwrap();
    let name = executable.file_stem().unwrap().to_string_lossy();
    #[cfg(windows)]
    if name == "gh" {
        fake_gh();
        return;
    }
    #[cfg(windows)]
    if name == "node" {
        fake_node(&executable);
        return;
    }
    #[cfg(windows)]
    if env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--redirect-doctor-cli")
    {
        redirect_doctor_cli();
        return;
    }
    let home = PathBuf::from(env::var_os("XDG_DATA_HOME").expect("data home"));
    let root = home.join("doctor-fixture");
    let scenario: Scenario =
        serde_json::from_slice(&fs::read(root.join(format!("{name}.json"))).unwrap()).unwrap();
    let marker = root.join(format!("{name}.fixed"));
    let args = env::args().skip(1).collect::<Vec<_>>();
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    let fixed = marker.exists();
    let stage_path = |stage: &str, event: &str| root.join(format!("{name}.{stage}.{event}"));
    let stage = match args.as_slice() {
        ["doctor", "check", "--json"] if fixed => "recheck",
        ["doctor", "check", "--json"] => "check",
        ["doctor", "check", "--full", "--json"] if scenario.full.is_some() => {
            if scenario.full_retry.is_some() && stage_path("full", "finished").exists() {
                "full-retry"
            } else {
                "full"
            }
        }
        ["doctor", "fix", _, "--digest", _, "--full"] => "fix-full",
        ["doctor", "fix", ..] => "fix",
        ["doctor", "manifest"] => "manifest",
        _ => process::exit(2),
    };
    let stage_identity = scenario.record_stages.then(|| {
        let pid = process::id();
        let start_time = subc_os::start_time(pid).expect("read fixture process start time");
        serde_json::json!({"pid": pid, "start_time": start_time, "image": executable}).to_string()
    });
    let record_stage = |event: &str| {
        if let Some(identity) = &stage_identity {
            fs::write(stage_path(stage, event), identity).unwrap();
        }
    };
    record_stage("started");
    if scenario.wait_stage.as_deref() == Some(stage) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !stage_path(stage, "release").exists() {
            assert!(Instant::now() < deadline, "fixture stage was not released");
            thread::sleep(Duration::from_millis(10));
        }
    }
    match args.as_slice() {
        ["doctor", "check", "--json"] => check(&scenario, &scenario.check, fixed),
        ["doctor", "check", "--full", "--json"] => {
            let check_kind = if stage == "full-retry" {
                scenario.full_retry.as_deref().unwrap()
            } else {
                scenario.full.as_deref().unwrap()
            };
            check(&scenario, check_kind, fixed);
        }
        ["doctor", "fix", id, "--digest", digest]
        | ["doctor", "fix", id, "--digest", digest, "--full"] => {
            if scenario.record_fix || scenario.clean_after_fix {
                fs::write(&marker, b"fix called\n").unwrap();
            }
            fix(&scenario, id, digest);
        }
        ["doctor", "manifest"] => manifest(&scenario),
        _ => process::exit(2),
    }
    record_stage("finished");
}

fn emit(value: impl serde::Serialize) {
    serde_json::to_writer(io::stdout().lock(), &value).unwrap();
    println!();
}

fn finding(scenario: &Scenario) -> Finding {
    let id = FindingId::new(&scenario.finding).unwrap();
    let steps = vec![
        "Reset the fixture state.".into(),
        "Verify the fixture state.".into(),
    ];
    let fix = if scenario.risky {
        Fix::risky
    } else {
        Fix::safe
    };
    let fix = fix(&id, scenario.needs_stop, steps, 4_000).unwrap();
    let mut evidence = vec![Evidence::new("attempts", EvidenceValue::Count(3)).unwrap()];
    if let Some(reference) = &scenario.private_ref {
        evidence.push(Evidence::new("reference", EvidenceValue::Ref(reference.clone())).unwrap());
    }
    if let Some(path) = &scenario.private_path {
        evidence.push(
            Evidence::new(
                "file",
                EvidenceValue::Path {
                    category: PathCategory::Data,
                    path: path.clone(),
                },
            )
            .unwrap(),
        );
    }
    Finding::new(
        id,
        scenario.severity,
        "The fixture state needs repair.",
        evidence,
        fix,
    )
    .unwrap()
}

fn check(scenario: &Scenario, check_kind: &str, fixed: bool) {
    thread::sleep(Duration::from_millis(scenario.check_sleep_ms));
    match check_kind {
        "findings" if !(fixed && scenario.clean_after_fix) => {
            emit(CheckOutput::new(vec![finding(scenario)]).unwrap())
        }
        "findings" | "quick" | "clean" => emit(CheckOutput::new(vec![]).unwrap()),
        "unknown" => process::exit(2),
        "usage_with_output" => {
            println!("check failed");
            process::exit(2);
        }
        "crash" => {
            println!("discarded fixture stdout");
            eprintln!("discarded fixture stderr");
            process::exit(41);
        }
        "sleep" => thread::sleep(Duration::from_secs(60)),
        "invalid" => println!("discarded fixture non-JSON"),
        "oversize" => {
            let _ = io::stdout().write_all(&vec![b'X'; MAX_OUTPUT_BYTES + 1]);
        }
        other => panic!("unknown check scenario: {other}"),
    }
}

#[cfg(windows)]
fn fake_node(executable: &std::path::Path) {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    let version = || {
        fs::read_to_string(executable.with_extension("version"))
            .unwrap_or_else(|_| "v22.19.0".into())
    };
    match args.as_slice() {
        ["-p", _] => println!("{}\n{}", executable.display(), version()),
        ["--version"] => println!("{}", version()),
        [_, "--version"] => println!("v1.6.0"),
        [_, "auth", "check", "--provider", "anthropic", "--json"] => {
            println!(r#"{{"status":"ready","provider":"anthropic"}}"#);
        }
        [_, "--system-prompt", ..] => {
            fs::write("launch-argv", format!("{}\n", args.join("\n"))).unwrap();
            let environment = env::vars()
                .map(|(name, value)| format!("{name}={value}\n"))
                .collect::<String>();
            fs::write("launch-env", environment).unwrap();
            let cwd = env::current_dir().unwrap();
            fs::write("launch-cwd", format!("{}\n", cwd.display())).unwrap();
            process::exit(23);
        }
        _ => process::exit(99),
    }
}

#[cfg(windows)]
fn fake_gh() {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let kind = &args[0];
    let host = env::var("GH_HOST").unwrap();
    assert_eq!(env::var("GH_PROMPT_DISABLED").unwrap(), "1");
    fs::write(
        format!("gh.{kind}"),
        format!("{host}\n{}\n", args.join("\n")),
    )
    .unwrap();
    let (auth, create, sleep_ms) =
        serde_json::from_slice::<(i32, i32, u64)>(&fs::read("gh.json").unwrap()).unwrap();
    let code = match kind.as_str() {
        "auth" => auth,
        "issue" => {
            let index = args.iter().position(|arg| arg == "--body-file").unwrap();
            fs::copy(&args[index + 1], "body.sent").unwrap();
            thread::sleep(Duration::from_millis(sleep_ms));
            println!("https://github.com/example/doctor-fixture/issues/1");
            create
        }
        _ => 99,
    };
    process::exit(code);
}

#[cfg(windows)]
#[tokio::main(flavor = "current_thread")]
async fn redirect_doctor_cli() {
    subc_jobobject::ensure_root_job().unwrap();
    subc_os::windows_console_test::restore_ctrl_c().expect("enable Ctrl-C in the test console");
    // Keep the launcher alive when the pseudoconsole sends a control event to
    // its console. ck receives the same event and owns cleanup and exit status.
    let _ctrl_c = tokio::signal::windows::ctrl_c().unwrap();
    let mut args = env::args_os().skip(2);
    let program = args.next().unwrap();
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdout(fs::File::create("stdout").unwrap())
        .kill_on_drop(true);
    if env::var_os("CK_TEST_REDIRECT_STDIN").is_some() {
        command.stdin(process::Stdio::piped());
    }
    let mut child = command.spawn().unwrap();
    if let Some(mut stdin) = child.stdin.take() {
        tokio::io::AsyncWriteExt::write_all(&mut stdin, b"yes\n")
            .await
            .unwrap();
    }
    process::exit(child.wait().await.unwrap().code().unwrap());
}

fn fix(scenario: &Scenario, id: &str, digest: &str) {
    let offered = finding(scenario);
    if id != offered.id().as_str() || digest != offered.fix().plan().unwrap().digest() {
        emit(FixOutput::refused("plan_changed").unwrap());
        return;
    }
    thread::sleep(Duration::from_millis(scenario.fix_sleep_ms));
    match scenario.fix.as_str() {
        "fixed" => emit(FixOutput::fixed()),
        "sleep" => {
            thread::sleep(Duration::from_secs(3));
            emit(FixOutput::fixed());
        }
        "invalid" => println!("discarded fixture non-JSON fix"),
        "failed" => emit(FixOutput::failed("repair_failed").unwrap()),
        "refused" | "module_running" => emit(FixOutput::refused("module_running").unwrap()),
        "plan_changed" => emit(FixOutput::refused("plan_changed").unwrap()),
        other => panic!("unknown fix scenario: {other}"),
    }
}

fn manifest(scenario: &Scenario) {
    thread::sleep(Duration::from_millis(scenario.manifest_sleep_ms));
    let repository = if scenario.manifest == "invalid_repository" {
        "https://github.com/example/doctor-fixture"
    } else {
        &scenario.repository
    };
    let manifest = Manifest::new(
        ModuleId::new("doctor-fixture").unwrap(),
        repository,
        vec![LogSource::new("*.jsonl").unwrap()],
        scenario
            .log_template
            .as_ref()
            .map(|text| Template::new("fixture", text, vec![]).unwrap())
            .into_iter()
            .collect(),
    )
    .unwrap();
    match scenario.manifest.as_str() {
        "valid" => io::stdout()
            .write_all(&manifest.to_canonical_bytes().unwrap())
            .unwrap(),
        "invalid_repository" => {
            let mut value = serde_json::to_value(manifest).unwrap();
            value["repository"] = "https://github.com/example/..".into();
            emit(value);
        }
        "sleep" | "hanging" => thread::sleep(Duration::from_secs(60)),
        other => panic!("unknown manifest scenario: {other}"),
    }
}
