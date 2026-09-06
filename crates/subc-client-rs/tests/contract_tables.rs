use std::path::{Path, PathBuf};

use serde::Deserialize;
use subc_client_rs::{
    is_retryable_route_open_code, RouteCloseDisposition, RouteCloseReason, DEFAULT_CALL_TIMEOUT,
    DEFAULT_LIVENESS_PROBE_WINDOW, DEFAULT_ROUTE_RETRY_DEADLINE,
};

#[derive(Debug, Deserialize)]
struct BudgetRow {
    name: String,
    ms: u64,
    owner: String,
    note: String,
}

#[derive(Debug, Deserialize)]
struct DecisionTables {
    route_open_retryable: std::collections::BTreeMap<String, String>,
    route_close_disposition: std::collections::BTreeMap<String, String>,
}

fn golden_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../subc-protocol/tests/golden")
        .join(format!("{name}.json"))
}

#[test]
fn client_budgets_match_contract_fixture() {
    let path = golden_path("budgets");
    assert!(
        path.exists(),
        "budgets fixture missing at {}; run from workspace root",
        path.display()
    );

    let content = std::fs::read_to_string(&path).expect("read budgets.json");
    let rows: Vec<BudgetRow> = serde_json::from_str(&content).expect("parse budgets.json");

    let mut found_retry_deadline = false;
    let mut found_probe_window = false;
    let mut found_request_timeout = false;

    for row in &rows {
        match row.name.as_str() {
            "route_open_retry_deadline" => {
                assert_eq!(
                    DEFAULT_ROUTE_RETRY_DEADLINE.as_millis() as u64,
                    row.ms,
                    "route_open_retry_deadline mismatch: DEFAULT_ROUTE_RETRY_DEADLINE must equal golden row"
                );
                assert_eq!(row.owner, "sdk");
                assert!(!row.note.is_empty());
                found_retry_deadline = true;
            }
            "liveness_probe_window" => {
                assert_eq!(
                    DEFAULT_LIVENESS_PROBE_WINDOW.as_millis() as u64,
                    row.ms,
                    "liveness_probe_window mismatch: DEFAULT_LIVENESS_PROBE_WINDOW must equal golden row"
                );
                assert_eq!(row.owner, "sdk");
                assert!(!row.note.is_empty());
                found_probe_window = true;
            }
            "request_timeout" => {
                assert_eq!(
                    DEFAULT_CALL_TIMEOUT.as_millis() as u64,
                    row.ms,
                    "request_timeout mismatch: DEFAULT_CALL_TIMEOUT must equal golden row"
                );
                assert_eq!(row.owner, "sdk");
                assert!(!row.note.is_empty());
                found_request_timeout = true;
            }
            _ => {}
        }
    }

    assert!(
        found_retry_deadline,
        "route_open_retry_deadline row missing in budgets.json"
    );
    assert!(
        found_probe_window,
        "liveness_probe_window row missing in budgets.json"
    );
    assert!(
        found_request_timeout,
        "request_timeout row missing in budgets.json"
    );
}

#[test]
fn sdk_route_open_retry_deadline_couples_to_daemon_drain_ceiling() {
    let path = golden_path("budgets");
    assert!(
        path.exists(),
        "budgets fixture missing at {}",
        path.display()
    );

    let content = std::fs::read_to_string(&path).expect("read budgets.json");
    let rows: Vec<BudgetRow> = serde_json::from_str(&content).expect("parse budgets.json");

    let drain_row = rows
        .iter()
        .find(|r| r.name == "drain_timeout")
        .expect("drain_timeout row missing in budgets.json");

    assert!(
        DEFAULT_ROUTE_RETRY_DEADLINE.as_millis() as u64 >= drain_row.ms,
        "coupling violation: SDK route-open retry deadline ({} ms) must be >= daemon drain ceiling ({} ms from fixture)",
        DEFAULT_ROUTE_RETRY_DEADLINE.as_millis(),
        drain_row.ms
    );
}

#[test]
fn execute_route_open_retryable_decision_table() {
    let path = golden_path("decision_tables");
    assert!(
        path.exists(),
        "decision_tables fixture missing at {}",
        path.display()
    );

    let content = std::fs::read_to_string(&path).expect("read decision_tables.json");
    let tables: DecisionTables =
        serde_json::from_str(&content).expect("parse decision_tables.json");

    assert!(
        !tables.route_open_retryable.is_empty(),
        "route_open_retryable table must not be empty"
    );

    for (code, expected_verdict) in &tables.route_open_retryable {
        let is_retryable = is_retryable_route_open_code(code);
        let actual_verdict = if is_retryable {
            "retryable"
        } else {
            "terminal"
        };
        assert_eq!(
            actual_verdict, expected_verdict,
            "route_open_retryable mismatch for code '{code}': expected {expected_verdict}, classifier produced {actual_verdict}"
        );
    }
}

#[test]
fn execute_route_close_disposition_decision_table() {
    let path = golden_path("decision_tables");
    assert!(
        path.exists(),
        "decision_tables fixture missing at {}",
        path.display()
    );

    let content = std::fs::read_to_string(&path).expect("read decision_tables.json");
    let tables: DecisionTables =
        serde_json::from_str(&content).expect("parse decision_tables.json");

    assert!(
        !tables.route_close_disposition.is_empty(),
        "route_close_disposition table must not be empty"
    );

    for (reason, expected_disposition) in &tables.route_close_disposition {
        let close_reason = RouteCloseReason::from_wire(reason);
        let actual_disposition = match close_reason.disposition() {
            RouteCloseDisposition::MayReopen => "may_reopen",
            RouteCloseDisposition::MustNotReopen => "must_not_reopen",
        };
        assert_eq!(
            actual_disposition, expected_disposition,
            "route_close_disposition mismatch for reason '{reason}': expected {expected_disposition}, classifier produced {actual_disposition}"
        );
    }
}
