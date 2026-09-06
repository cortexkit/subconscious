use std::path::{Path, PathBuf};

use serde::Deserialize;

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
fn daemon_budgets_match_contract_fixture() {
    let path = golden_path("budgets");
    assert!(
        path.exists(),
        "budgets fixture missing at {}; run from workspace root",
        path.display()
    );

    let content = std::fs::read_to_string(&path).expect("read budgets.json");
    let rows: Vec<BudgetRow> = serde_json::from_str(&content).expect("parse budgets.json");

    let mut found_drain = false;
    let mut found_relay = false;
    let mut found_auth = false;

    for row in &rows {
        match row.name.as_str() {
            "drain_timeout" => {
                assert_eq!(
                    subc_core::DEFAULT_DRAIN_TIMEOUT.as_millis() as u64,
                    row.ms,
                    "drain_timeout mismatch: DEFAULT_DRAIN_TIMEOUT must equal golden row"
                );
                assert_eq!(row.owner, "daemon");
                assert!(!row.note.is_empty());
                found_drain = true;
            }
            "route_bind_relay_timeout" => {
                assert_eq!(
                    subc_core::DEFAULT_ROUTE_BIND_RELAY_TIMEOUT.as_millis() as u64,
                    row.ms,
                    "route_bind_relay_timeout mismatch: DEFAULT_ROUTE_BIND_RELAY_TIMEOUT must equal golden row"
                );
                assert_eq!(row.owner, "daemon");
                assert!(!row.note.is_empty());
                found_relay = true;
            }
            "auth_deadline" => {
                assert_eq!(
                    subc_core::DEFAULT_AUTH_DEADLINE.as_millis() as u64,
                    row.ms,
                    "auth_deadline mismatch: DEFAULT_AUTH_DEADLINE must equal golden row"
                );
                assert_eq!(row.owner, "daemon");
                assert!(!row.note.is_empty());
                found_auth = true;
            }
            _ => {}
        }
    }

    assert!(found_drain, "drain_timeout row missing in budgets.json");
    assert!(
        found_relay,
        "route_bind_relay_timeout row missing in budgets.json"
    );
    assert!(found_auth, "auth_deadline row missing in budgets.json");
}

#[test]
fn daemon_route_open_error_codes_are_complete_in_decision_tables() {
    let path = golden_path("decision_tables");
    assert!(
        path.exists(),
        "decision_tables fixture missing at {}; run from workspace root",
        path.display()
    );

    let content = std::fs::read_to_string(&path).expect("read decision_tables.json");
    let tables: DecisionTables =
        serde_json::from_str(&content).expect("parse decision_tables.json");

    assert!(
        !tables.route_close_disposition.is_empty(),
        "route_close_disposition must not be empty in decision_tables.json"
    );

    // All error codes in subc_protocol::error_codes must be present in the decision table.
    let protocol_codes = [
        subc_protocol::error_codes::UNKNOWN_MODULE,
        subc_protocol::error_codes::MODULE_REMOVED,
        subc_protocol::error_codes::MODULE_RELOADING,
        subc_protocol::error_codes::MODULE_WARMING,
        subc_protocol::error_codes::TARGET_UNAVAILABLE,
        subc_protocol::error_codes::MODULE_TIMEOUT,
    ];
    for code in protocol_codes {
        assert!(
            tables.route_open_retryable.contains_key(code),
            "subc_protocol::error_codes '{code}' missing from route_open_retryable in decision_tables.json"
        );
    }

    // Every refusal code produced by daemon route.open refusal sites must be represented.
    let daemon_refusal_codes = [
        "admission_facts_not_permitted",
        "admission_facts_target_not_allowed",
        "bad_consumer_identity",
        "capability_forbidden",
        "forwarding_error",
        "invalid_project_root",
        "module_reloading",
        "module_removed",
        "module_timeout",
        "module_warming",
        "op_not_allowed",
        "route_limit",
        "target_unavailable",
        "unknown_module",
    ];
    for code in daemon_refusal_codes {
        assert!(
            tables.route_open_retryable.contains_key(code),
            "daemon route.open refusal code '{code}' missing from route_open_retryable in decision_tables.json"
        );
    }
}
