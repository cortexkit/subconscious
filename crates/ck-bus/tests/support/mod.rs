#![cfg(unix)]

use std::{
    fs,
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::Value;
use subc_control::{
    ClientControlRequest as Request, ClientControlResponse as Response, ModuleProtocol,
    SupervisorEntry, TerminalEntry,
};

use crate::harness::{control, daemon::AcceptanceRun};

pub const SERVER: &str = "nats-server";

pub fn version_floor() {
    let version = include_str!("../../../subc-daemon/Cargo.toml")
        .lines()
        .find_map(|line| {
            line.strip_prefix("version = \"")
                .and_then(|v| v.strip_suffix('"'))
        })
        .expect("fire-time subc-daemon package version must exist");
    let parts: Vec<u64> = version
        .split('.')
        .map(|p| p.parse().expect("numeric daemon version"))
        .collect();
    assert!(
        parts.as_slice() >= [0, 20, 5].as_slice(),
        "subc-daemon {version} is below 0.20.5"
    );
    eprintln!("fire-time crates/subc-daemon/Cargo.toml version={version}; slice-0 FIRE-TIME-RECORD.md records TerminalEntry.at_ms/exit_code/exit_signal and SupervisorModuleProvenance.daemon_observed.pid");
}

pub fn declared_args(id: &str) -> Vec<String> {
    let value = crate::harness::config::template_value();
    let args = value["modules"][id]["args"]
        .as_array()
        .expect("declared args list");
    let args: Vec<String> = args
        .iter()
        .map(|v| v.as_str().expect("string arg").to_owned())
        .collect();
    assert!(
        !args.is_empty()
            && args
                .iter()
                .all(|a| !a.is_empty() && !a.contains(char::is_whitespace)),
        "{id} requires nonempty whitespace-free argv"
    );
    eprintln!("{id} final declared argument={:?}", args.last());
    args
}

/// Every stand-in A1 tears down. The first two share one script under two
/// declarations; the third is the negative control that ignores SIGTERM.
pub const STANDINS: [&str; 3] = [
    "standin-none",
    "standin-default-protocol",
    "standin-ignores-term",
];

pub fn standin_preconditions() {
    use std::os::unix::fs::PermissionsExt;
    for id in STANDINS {
        let path = standin_program(id);
        assert!(
            path.is_file(),
            "stand-in script for {id} must exist: {}",
            path.display()
        );
        assert_ne!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o111,
            0,
            "stand-in for {id} must be executable"
        );
        declared_args(id);
    }
}

/// The stand-in script a fixture block declares, resolved under this crate's
/// `tests/support`. The fixture names it relative to the workspace root, which
/// is not the directory the in-process daemon runs from.
fn standin_program(id: &str) -> std::path::PathBuf {
    let value = crate::harness::config::template_value();
    let declared = value["modules"][id]["program"]
        .as_str()
        .unwrap_or_else(|| panic!("{id} must declare a program"));
    let file_name = Path::new(declared)
        .file_name()
        .unwrap_or_else(|| panic!("{id} program {declared} has no file name"));
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/support")
        .join(file_name)
}

pub async fn enable(run: &AcceptanceRun, id: &str) {
    let mut config: Value = serde_json::from_slice(&fs::read(&run.config_file).unwrap()).unwrap();
    let block = &mut config["modules"][id];
    assert!(
        block.is_object(),
        "missing fixture block {id}; fixture belongs to slice 0"
    );
    if id == SERVER {
        let binary = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join("nats-server"))
            .find(|path| path.is_file())
            .or_else(|| {
                Path::new("/opt/homebrew/bin/nats-server")
                    .is_file()
                    .then(|| Path::new("/opt/homebrew/bin/nats-server").to_path_buf())
            })
            .expect("real nats-server binary required on unix");
        block["program"] = binary
            .to_str()
            .expect("nats-server path must be UTF-8")
            .into();
    } else {
        block["program"] = standin_program(id).to_str().unwrap().into();
    }
    block["enabled"] = true.into();
    // The acceptance daemon is in-process; child-specific homes belong on its module declaration.
    block["env"] = serde_json::json!({"XDG_DATA_HOME": run.root.join("data"), "XDG_RUNTIME_DIR": run.root.join("run"), "XDG_CONFIG_HOME": run.root.join("config")});
    fs::write(
        &run.config_file,
        serde_json::to_vec_pretty(&config).unwrap(),
    )
    .unwrap();
    let response = control::response(
        &run.connection_file,
        Request::SupervisorRescan { preview: false },
    )
    .await;
    let Response::SupervisorRescan { .. } = response else {
        panic!("supervisor.rescan response: {response:?}")
    };
    wait_running(run, id).await;
}

pub async fn entry(run: &AcceptanceRun, id: &str) -> SupervisorEntry {
    let response = control::response(&run.connection_file, Request::SupervisorList {}).await;
    let Response::SupervisorList { modules, .. } = response else {
        panic!("supervisor.list response: {response:?}")
    };
    modules
        .into_iter()
        .find(|m| m.module_id == id)
        .unwrap_or_else(|| panic!("supervisor.list missing {id}"))
}

pub async fn wait_running(run: &AcceptanceRun, id: &str) -> SupervisorEntry {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let current = entry(run, id).await;
        if current.state == "running" {
            return current;
        }
        assert!(Instant::now() < deadline, "{id} never ran: {current:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub async fn pid(run: &AcceptanceRun, id: &str) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let reply = control::rpc(
            &run.connection_file,
            Request::SupervisorProvenance {
                module_id: Some(id.into()),
            },
        )
        .await;
        let response = match reply {
            control::ControlReply::Error(error) => panic!(
                "supervisor.provenance narrowed to {id} refused: {}",
                error.code
            ),
            control::ControlReply::Response(response) => response,
        };
        let Response::SupervisorProvenance { modules, .. } = response else {
            panic!("unexpected provenance reply {response:?}")
        };
        let matching: Vec<_> = modules.iter().filter(|m| m.module_id == id).collect();
        assert_eq!(
            matching.len(),
            1,
            "narrowed supervisor.provenance {id} reply: {modules:?}"
        );
        if let Some(pid) = matching[0].daemon_observed.pid {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "supervisor.provenance {id} pid absent for 2 s: {modules:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

pub async fn terminals(run: &AcceptanceRun, id: &str) -> Vec<TerminalEntry> {
    let response = control::response(
        &run.connection_file,
        Request::SupervisorTerminals {
            module_id: id.into(),
        },
    )
    .await;
    let Response::SupervisorTerminals { terminals, .. } = response else {
        panic!("supervisor.terminals response {response:?}")
    };
    assert_eq!(
        terminals.journal_write_failures, 0,
        "terminal journal must be writable"
    );
    terminals.entries
}

pub async fn new_terminal(run: &AcceptanceRun, id: &str, prior: usize) -> TerminalEntry {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let records = terminals(run, id).await;
        assert!(
            records.len() <= prior + 1,
            "unexpected earlier terminal record for {id}: {records:?}"
        );
        if records.len() == prior + 1 {
            let journal = fs::read_to_string(run.root.join("run/terminals.jsonl"))
                .expect("fixture journal must exist");
            let matches: Vec<TerminalEntry> = journal
                .lines()
                .filter_map(|line| {
                    let value: Value = serde_json::from_str(line).ok()?;
                    (value["module_id"] == id).then(|| {
                        serde_json::from_value(value)
                            .expect("recorded TerminalEntry fields must decode")
                    })
                })
                .collect();
            assert_eq!(
                matches, records,
                "journal and supervisor.terminals must agree for {id}"
            );
            return records[prior].clone();
        }
        assert!(Instant::now() < deadline, "no terminal record for {id}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Wait for a stand-in's readiness file, written once its signal handling is
/// installed.
pub async fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.is_file() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub fn budget(entry: &SupervisorEntry) -> u64 {
    entry
        .drain_timeout_ms
        .expect("supervisor.list must report effective drain_timeout_ms")
}

pub fn elapsed(record: &TerminalEntry, start: u64) -> u64 {
    assert!(
        record.at_ms >= start,
        "terminal timestamp predates teardown"
    );
    record.at_ms - start
}

pub fn protocol(entry: &SupervisorEntry, expected: ModuleProtocol) {
    assert_eq!(
        entry.protocol, expected,
        "supervisor.list effective protocol"
    );
}
