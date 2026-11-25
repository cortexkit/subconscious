use serde_json::{json, Value};
use subc_control_published::{ClientControlResponse, SupervisorEntry};

// The live daemon test compares its canonical JSON bytes to this same file.
const LIST: &[u8] =
    include_bytes!("../../subc-daemon/tests/golden/supervisor_list_resource_diagnostics.json");

#[test]
fn currently_published_control_decodes_the_daemons_list_and_status_replies() {
    let mut body: Value = serde_json::from_slice(LIST).unwrap();
    assert_eq!(body["modules"][0]["cpu_last_hour"]["bound"], "lower");

    let old: ClientControlResponse = serde_json::from_slice(LIST).unwrap();
    let encoded = serde_json::to_value(old).unwrap();
    assert_eq!(encoded["generation"], body["generation"]);
    assert_eq!(encoded["modules"][0]["module_id"], "resource-module");
    assert!(encoded.get("daemon").is_none());
    assert!(encoded.get("agent_commands").is_none());
    assert!(encoded["modules"][0].get("tree").is_none());
    assert!(encoded["modules"][0].get("cpu_last_hour").is_none());
    // The command-line client's module status reads this list entry.
    let status: SupervisorEntry = serde_json::from_value(body["modules"][0].clone()).unwrap();
    assert_eq!(status.module_id, "resource-module");

    body["future"] = json!({"unexpected": [1, false]});
    body["modules"][0]["future"] = json!(false);
    let future: ClientControlResponse = serde_json::from_value(body).unwrap();
    assert_eq!(serde_json::to_value(future).unwrap(), encoded);
}
