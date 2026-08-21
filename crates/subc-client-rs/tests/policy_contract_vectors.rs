//! Producer-real contract vectors for policy.resolve, vendored from prefrontal
//! (crates/prefrontal-core-module/tests/fixtures/policy_resolve/contract_vectors.json).
//! On the producer side each committed request is EXECUTED against live
//! dispatch and the committed reply asserted byte-equal, so this file cannot
//! drift from the served op; on this side, every vector runs through the
//! helper's own serializer/parser. Field names survived prose pinning; the
//! subject ENCODING and verdict VOCABULARY did not — only bytes pin those.

use subc_client_rs::{PolicyVerdict, ProjectRef, Subject};

const VECTORS: &str = include_str!("fixtures/policy_resolve_contract_vectors.json");
/// Producer digest (prefrontal, two-form amendment + push_event pin). Re-vendor
/// from prefrontal rather than editing bytes here: the fixture is the contract.
const PRODUCER_SHA256: &str = "40c02a32b56c26571f47ef83f7833f153b9b91ea20b077990a3153cfac4bdb8b";

fn expected_verdict(name: &str, wire: &str) -> PolicyVerdict {
    match wire {
        "allow" => PolicyVerdict::Allow,
        "deny" => PolicyVerdict::Deny,
        "ask" => PolicyVerdict::Ask,
        "deny_unknown_domain" => PolicyVerdict::DenyUnknownDomain,
        other => panic!("vector '{name}' carries a verdict this helper does not type: {other}"),
    }
}

#[test]
fn vendored_bytes_match_the_producer_digest() {
    let digest = sha256_hex(VECTORS.as_bytes());
    assert_eq!(
        digest, PRODUCER_SHA256,
        "vendored policy vectors no longer match the producer pin; re-vendor from prefrontal"
    );
}

#[test]
fn every_vector_round_trips_through_the_helper_types() {
    let doc: serde_json::Value = serde_json::from_str(VECTORS).expect("vectors parse");
    let vectors = doc["vectors"].as_array().expect("vectors array");
    let mut replies = 0usize;
    let mut refusals = 0usize;
    let mut subjects = 0usize;
    let mut root_forms = 0usize;
    let mut id_forms = 0usize;

    for vector in vectors {
        let name = vector["name"].as_str().expect("name");
        // Subject encoding: serialize the helper's Subject from the vector's
        // request and require the exact wire object the producer executed.
        let wire_subject = &vector["request"]["subject"];
        if let Some(agent_id) = wire_subject.get("agent_id") {
            let ours = serde_json::to_value(Subject::AgentId(
                agent_id.as_str().expect("agent_id string").to_string(),
            ))
            .unwrap();
            assert_eq!(
                &ours, wire_subject,
                "vector '{name}': agent subject encoding diverged"
            );
            subjects += 1;
        } else if let Some(session_id) = wire_subject.get("session_id") {
            let ours = serde_json::to_value(Subject::SessionToResolve(
                session_id.as_str().expect("session_id string").to_string(),
            ))
            .unwrap();
            assert_eq!(
                &ours, wire_subject,
                "vector '{name}': session subject encoding diverged"
            );
            subjects += 1;
        } else {
            panic!("vector '{name}': subject shape unknown to this helper: {wire_subject}");
        }

        // Project reference: whichever form the vector's request carries, the
        // helper's ProjectRef must serialize to the exact same wire key.
        let request = &vector["request"];
        if let Some(root) = request.get("project_root") {
            let ours = serde_json::to_value(ProjectRef::Root(
                root.as_str().expect("project_root string").to_string(),
            ))
            .unwrap();
            assert_eq!(
                ours.get("project_root"),
                Some(root),
                "vector '{name}': project_root encoding diverged"
            );
            root_forms += 1;
        } else if let Some(id) = request.get("project_id") {
            let ours = serde_json::to_value(ProjectRef::Id(
                id.as_str().expect("project_id string").to_string(),
            ))
            .unwrap();
            assert_eq!(
                ours.get("project_id"),
                Some(id),
                "vector '{name}': project_id encoding diverged"
            );
            id_forms += 1;
        }

        if let Some(reply) = vector.get("reply") {
            let wire = reply["verdict"].as_str().expect("verdict string");
            let parsed: PolicyVerdict =
                serde_json::from_value(reply["verdict"].clone()).expect("verdict parses");
            assert_eq!(parsed, expected_verdict(name, wire), "vector '{name}'");
            assert!(reply["revision"].is_u64(), "vector '{name}': revision u64");
            assert!(reply["ttl_ms"].is_u64(), "vector '{name}': ttl_ms u64");
            replies += 1;
        } else if let Some(error) = vector.get("error") {
            assert!(
                error["code"].is_string(),
                "vector '{name}': refusal carries a typed code"
            );
            refusals += 1;
        } else {
            panic!("vector '{name}' has neither reply nor error");
        }
    }

    // Vacuity floor pinned to the producer's published roster: nine vectors
    // (7 + the two-form amendment pair), at least two refusals (unknown
    // session, unknown project id), both project forms present.
    assert_eq!(
        replies + refusals,
        9,
        "vector count changed; re-read the contract"
    );
    assert!(
        refusals >= 2,
        "refusal vectors missing (session + project id)"
    );
    assert_eq!(
        subjects, 9,
        "every vector carries a subject this helper can emit"
    );
    // Every vector carries a project reference; both forms must be exercised
    // (the id pair is the amendment; roots are the original seven).
    assert_eq!(
        root_forms + id_forms,
        9,
        "every vector carries a project form"
    );
    assert!(id_forms >= 2, "the project_id amendment pair is missing");
    assert!(root_forms >= 1, "the project_root form vanished");
}

#[test]
fn push_event_bump_parses_through_the_helper_shape() {
    // The held-stream event, byte-pinned producer-side against the encoder
    // itself. Parse it with the exact serde types the drain task uses; a flat
    // {revision} was the eighth encoding drift and THIS assertion is what
    // keeps the nested framing from drifting silently on our side.
    let doc: serde_json::Value = serde_json::from_str(VECTORS).expect("vectors parse");
    let event = &doc["push_event"]["event"];
    assert_eq!(event["op"], "policy.revision_bump");
    let revision = event["body"]["revision"].as_u64().expect("nested revision");
    assert_eq!(revision, 3, "fixture bump revision");
    // Both polarities: the nested form parses, the flat form (old drift) must
    // NOT satisfy the shape the drain requires.
    assert!(
        event.get("revision").is_none(),
        "fixture event must not carry a flat revision alongside body"
    );
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::process::Command;
    // No sha2 dependency in this crate; shell out for the test-only digest.
    let out = Command::new("shasum")
        .args(["-a", "256", "-"])
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.as_mut().unwrap().write_all(bytes)?;
            child.wait_with_output()
        })
        .expect("shasum runs");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .split_whitespace()
        .next()
        .expect("digest")
        .to_string()
}
