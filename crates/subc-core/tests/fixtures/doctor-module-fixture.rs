#![forbid(unsafe_code)]

use std::{
    env, fs,
    io::{self, Write},
    path::PathBuf,
    process, thread,
    time::Duration,
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
    let name = executable.file_name().unwrap().to_string_lossy();
    let home = PathBuf::from(env::var_os("XDG_DATA_HOME").expect("data home"));
    let root = home.join("doctor-fixture");
    let scenario: Scenario =
        serde_json::from_slice(&fs::read(root.join(format!("{name}.json"))).unwrap()).unwrap();
    let marker = root.join(format!("{name}.fixed"));
    let args = env::args().skip(1).collect::<Vec<_>>();
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    let fixed = marker.exists();
    let stage = match args.as_slice() {
        ["doctor", "check", "--json"] if fixed => "recheck",
        ["doctor", "check", "--json"] => "check",
        ["doctor", "fix", ..] => "fix",
        ["doctor", "manifest"] => "manifest",
        _ => process::exit(2),
    };
    let record_stage = |event: &str| {
        if scenario.record_stages {
            fs::write(
                root.join(format!("{name}.{stage}.{event}")),
                process::id().to_string(),
            )
            .unwrap();
        }
    };
    record_stage("started");
    match args.as_slice() {
        ["doctor", "check", "--json"] => check(&scenario, fixed),
        ["doctor", "fix", id, "--digest", digest] => {
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

fn check(scenario: &Scenario, fixed: bool) {
    thread::sleep(Duration::from_millis(scenario.check_sleep_ms));
    match scenario.check.as_str() {
        "findings" if !(fixed && scenario.clean_after_fix) => {
            emit(CheckOutput::new(vec![finding(scenario)]).unwrap())
        }
        "findings" | "quick" | "clean" => emit(CheckOutput::new(vec![]).unwrap()),
        "unknown" => process::exit(2),
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
