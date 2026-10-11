use std::fmt::Debug;

use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use subc_control::*;

fn round_trip<T>(body: Value) -> T
where
    T: DeserializeOwned + Serialize + PartialEq + Debug,
{
    let decoded: T = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(serde_json::to_value(&decoded).unwrap(), body);
    let encoded = serde_json::to_string(&decoded).unwrap();
    assert_eq!(serde_json::from_str::<T>(&encoded).unwrap(), decoded);
    decoded
}

fn cursor() -> Value {
    json!({"daemon_incarnation": "daemon-b", "seq": 41})
}

fn actor() -> Value {
    json!({
        "principal": {"kind": "reserved", "module_id": "prefrontal"},
        "scope_owner": {"kind": "reserved", "module_id": "prefrontal"},
        "agent_id": "agent-1", "run_id": "run-1", "flow_id": "flow-1",
        "project_root": "workspace"
    })
}

fn entry() -> Value {
    json!({
        "seq": 41, "at_ms": 1000, "daemon_incarnation": "daemon-a",
        "module": "prefrontal", "kind": "prefrontal.worker_settled",
        "summary": "Worker settled", "subject": {"type": "worker", "id": "worker-1"},
        "details": {"worker_id": "worker-1", "result": {"ok": true}},
        "actor": actor()
    })
}

#[test]
fn activity_operation_constants_match_wire_names() {
    assert_eq!(ops::ACTIVITY_RECORD, "activity.record");
    assert_eq!(ops::ACTIVITY_QUERY, "activity.query");
    assert_eq!(ops::ACTIVITY_SUBSCRIBE, "activity.subscribe");
    assert_eq!(ActivityRecordRequest::OP, "activity.record");
    assert_eq!(ActivityRecordResponse::OP, "activity.record");
    assert_eq!(ActivityQueryRequest::OP, "activity.query");
    assert_eq!(ActivityQueryResponse::OP, "activity.query");
    assert_eq!(ActivitySubscribeRequest::OP, "activity.subscribe");
    assert_eq!(ActivitySubscribeResponse::OP, "activity.subscribe");
}

#[test]
fn activity_bodies_dispatch_by_op() {
    for body in [
        json!({"op": "activity.record", "kind": "prefrontal.worker_settled", "summary": "Worker settled"}),
        json!({"op": "activity.query", "limit": 10}),
        json!({"op": "activity.subscribe", "since": cursor()}),
    ] {
        match body["op"].as_str().unwrap() {
            ActivityRecordRequest::OP => {
                round_trip::<ActivityRecordRequest>(body);
            }
            ActivityQueryRequest::OP => {
                round_trip::<ActivityQueryRequest>(body);
            }
            ActivitySubscribeRequest::OP => {
                round_trip::<ActivitySubscribeRequest>(body);
            }
            other => panic!("unrecognized activity request: {other}"),
        }
    }
    for body in [
        json!({"op": "activity.record", "seq": 41}),
        json!({"op": "activity.query", "entries": [], "head": cursor()}),
        json!({"op": "activity.subscribe", "entry": entry()}),
    ] {
        match body["op"].as_str().unwrap() {
            ActivityRecordResponse::OP => {
                round_trip::<ActivityRecordResponse>(body);
            }
            ActivityQueryResponse::OP => {
                round_trip::<ActivityQueryResponse>(body);
            }
            ActivitySubscribeResponse::OP => {
                round_trip::<ActivitySubscribeResponse>(body);
            }
            other => panic!("unrecognized activity response: {other}"),
        }
    }
}

#[test]
fn activity_entry_and_actor_round_trip() {
    let stored = round_trip::<ActivityEntry>(entry());
    assert_eq!(stored.seq, 41);
    assert_eq!(stored.at_ms, 1000);
    assert_eq!(stored.daemon_incarnation, "daemon-a");
    round_trip::<ActivityActor>(actor());
    round_trip::<ActivityActor>(json!({"principal": {"kind": "direct"}}));
    round_trip::<ActivitySubject>(json!({"type": "worker", "id": "worker-1"}));
    let serialized = serde_json::to_value(&stored).unwrap();
    assert!(serialized.get("prev").is_none());
    assert!(serialized["details"].is_object());
    let mut without_context = entry();
    without_context.as_object_mut().unwrap().remove("subject");
    without_context["details"] = json!({});
    without_context["actor"] =
        json!({"principal": {"kind": "reserved", "module_id": "prefrontal"}});
    round_trip::<ActivityEntry>(without_context);
    let mut future_entry = entry();
    future_entry["prev"] = json!("unused");
    let decoded: ActivityEntry = serde_json::from_value(future_entry).unwrap();
    assert!(serde_json::to_value(decoded).unwrap().get("prev").is_none());
    for details in [json!("not an object"), json!([]), json!(null), json!(42)] {
        let mut malformed = entry();
        malformed["details"] = details;
        assert!(serde_json::from_value::<ActivityEntry>(malformed).is_err());
    }
    for subject in [
        json!({"type": "worker"}),
        json!({"id": "worker-1"}),
        json!({"type": "worker", "id": "worker-1", "extra": true}),
    ] {
        assert!(serde_json::from_value::<ActivitySubject>(subject).is_err());
    }
}

#[test]
fn activity_cursor_and_query_head_round_trip() {
    round_trip::<ActivityCursor>(cursor());
    round_trip::<ActivityQueryResponse>(json!({
        "op": "activity.query", "entries": [entry()], "cursor": 41, "head": cursor()
    }));
    round_trip::<ActivityQueryResponse>(json!({
        "op": "activity.query", "entries": [], "head": cursor()
    }));
    assert!(serde_json::from_value::<ActivityQueryResponse>(json!({
        "op": "activity.query", "entries": []
    }))
    .is_err());
    assert!(serde_json::from_value::<ActivityQueryResponse>(json!({
        "op": "activity.query", "entries": [], "head": {"seq": 41}
    }))
    .is_err());
    assert!(
        serde_json::from_value::<ActivityCursor>(json!({"daemon_incarnation": "daemon-b"}))
            .is_err()
    );
}

#[test]
fn activity_record_request_round_trip_without_actor() {
    let minimal = json!({"op": "activity.record", "kind": "prefrontal.worker_settled", "summary": "Worker settled"});
    let request = round_trip::<ActivityRecordRequest>(minimal.clone());
    assert!(request.subject.is_none() && request.details.is_none() && request.on.is_none());
    round_trip::<ActivityRecordResponse>(json!({"op": "activity.record", "seq": 41}));
    for on in [
        json!({"route": {"channel": 7, "epoch": 3}}),
        json!({"scope": {"ref": "scope-1", "epoch": 4}}),
    ] {
        round_trip::<ActivityOn>(on.clone());
        let mut body = minimal.clone();
        body["on"] = on;
        body["subject"] = json!({"type": "worker", "id": "worker-1"});
        body["details"] = json!({"worker_id": "worker-1"});
        round_trip::<ActivityRecordRequest>(body);
    }
    round_trip::<ActivityRoute>(json!({"channel": 7, "epoch": 3}));
    round_trip::<ActivityScope>(json!({"ref": "scope-1", "epoch": 4}));
    let mut forged = minimal.clone();
    forged["actor"] = actor();
    assert!(serde_json::from_value::<ActivityRecordRequest>(forged).is_err());
    for details in [json!([]), json!("bad"), json!(0), json!(null)] {
        let mut malformed = minimal.clone();
        malformed["details"] = details;
        assert!(serde_json::from_value::<ActivityRecordRequest>(malformed).is_err());
    }
    for on in [
        json!({"route": {"channel": 7}}),
        json!({"scope": {"ref": "scope-1"}}),
        json!({"scope": {"ref": "scope-1", "epoch": 4, "actor": actor()}}),
        json!({"route": {"channel": 7, "epoch": 3}, "scope": {"ref": "scope-1", "epoch": 4}}),
    ] {
        assert!(serde_json::from_value::<ActivityOn>(on).is_err());
    }
}

#[test]
fn activity_query_request_round_trip() {
    let request = round_trip::<ActivityQueryRequest>(json!({"op": "activity.query", "limit": 10}));
    assert_eq!(request.limit, 10);
    assert!(request.cursor.is_none());
    round_trip::<ActivityQueryRequest>(json!({
        "op": "activity.query", "agent": "agent-1", "project": "workspace",
        "module": "prefrontal", "kind": "prefrontal.worker_settled",
        "since_ms": 500, "until_ms": 1000, "limit": 20, "cursor": 41
    }));
    assert!(
        serde_json::from_value::<ActivityQueryRequest>(json!({"op": "activity.query"})).is_err()
    );
    assert!(serde_json::from_value::<ActivityQueryRequest>(
        json!({"op": "activity.query", "limit": 10, "cursor": cursor()})
    )
    .is_err());
}

#[test]
fn activity_subscribe_round_trip() {
    let request = round_trip::<ActivitySubscribeRequest>(json!({"op": "activity.subscribe"}));
    assert!(request.since.is_none());
    round_trip::<ActivitySubscribeRequest>(json!({"op": "activity.subscribe", "since": cursor()}));
    round_trip::<ActivitySubscribeResponse>(json!({"op": "activity.subscribe", "entry": entry()}));
    assert!(serde_json::from_value::<ActivitySubscribeRequest>(
        json!({"op": "activity.subscribe", "since": 41})
    )
    .is_err());
}

#[test]
fn activity_refusal_codes_are_stable() {
    assert_eq!(
        [
            ACTIVITY_KIND_FORBIDDEN,
            ACTIVITY_ACTOR_UNKNOWN,
            ACTIVITY_RATE_LIMITED,
            ACTIVITY_READ_FORBIDDEN,
            ACTIVITY_TOO_LARGE,
            ACTIVITY_MALFORMED,
            ACTIVITY_UNAVAILABLE,
            ACTIVITY_CURSOR_INCARNATION_MISMATCH,
            ACTIVITY_CURSOR_TOO_OLD,
            ACTIVITY_SUBSCRIBER_LAGGED
        ],
        [
            "activity_kind_forbidden",
            "activity_actor_unknown",
            "activity_rate_limited",
            "activity_read_forbidden",
            "activity_too_large",
            "activity_malformed",
            "activity_unavailable",
            "activity_cursor_incarnation_mismatch",
            "activity_cursor_too_old",
            "activity_subscriber_lagged"
        ]
    );
}

#[test]
fn activity_capability_is_versioned() {
    assert_eq!(ACTIVITY_LEDGER_CAPABILITY, "activity-ledger/v1");
}

#[test]
fn legacy_control_enums_remain_unchanged() {
    assert_eq!(
        serde_json::from_value::<ClientControlRequest>(json!({"op": "server.describe"})).unwrap(),
        ClientControlRequest::ServerDescribe {}
    );
    round_trip::<ClientControlResponse>(
        json!({"op": "route.open", "route_channel": 7, "route_epoch": 3}),
    );
    for body in [
        json!({"op": "activity.record", "kind": "prefrontal.worker_settled", "summary": "Worker settled", "seq": 41}),
        json!({"op": "activity.query", "limit": 10, "entries": [], "head": cursor()}),
        json!({"op": "activity.subscribe", "entry": entry()}),
    ] {
        assert!(serde_json::from_value::<ClientControlRequest>(body.clone()).is_err());
        assert!(serde_json::from_value::<ClientControlResponse>(body).is_err());
    }
}

#[test]
fn release_metadata_is_versioned_and_logged() {
    let version = env!("CARGO_PKG_VERSION");
    let parts: Vec<u32> = version
        .split('.')
        .map(|part| part.parse().unwrap())
        .collect();
    assert!(
        parts.as_slice() > [0, 29, 3].as_slice(),
        "activity types must not ship as the old release"
    );
    let changelog = include_str!("../CHANGELOG.md");
    let release_heading = format!("## {version}\n");
    assert!(
        changelog.contains(&release_heading),
        "current version has a changelog entry"
    );
    let entry = changelog
        .split("## 0.29.4\n")
        .nth(1)
        .expect("activity release has a changelog entry")
        .split("\n## ")
        .next()
        .unwrap();
    assert!(entry.contains("activity"));
    let lock = include_str!("../../../Cargo.lock");
    let locked = lock
        .split("name = \"subc-control\"\n")
        .nth(1)
        .expect("control has a lock entry")
        .lines()
        .next()
        .unwrap();
    assert_eq!(locked, format!("version = \"{version}\""));
}

#[test]
fn activity_bodies_require_matching_op() {
    fn rejects_wrong_op<T: DeserializeOwned>(mut body: Value) {
        body["op"] = json!("not.activity");
        assert!(serde_json::from_value::<T>(body.clone()).is_err());
        body.as_object_mut().unwrap().remove("op");
        assert!(serde_json::from_value::<T>(body).is_err());
    }
    rejects_wrong_op::<ActivityRecordRequest>(
        json!({"kind": "prefrontal.worker_settled", "summary": "Worker settled"}),
    );
    rejects_wrong_op::<ActivityRecordResponse>(json!({"seq": 41}));
    rejects_wrong_op::<ActivityQueryRequest>(json!({"limit": 10}));
    rejects_wrong_op::<ActivityQueryResponse>(json!({"entries": [], "head": cursor()}));
    rejects_wrong_op::<ActivitySubscribeRequest>(json!({}));
    rejects_wrong_op::<ActivitySubscribeResponse>(json!({"entry": entry()}));
}
