use super::*;
use subc_client_rs::{ActivityCursor, ActivityQueryRequest, ActivityRefusal, ActivityStreamError};

const MODULE: &str = "subc-client-rs-activity";

fn query() -> ActivityQueryRequest {
    serde_json::from_value(json!({"op":"activity.query", "module":MODULE, "limit":10})).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activity_supervised_reader_is_forbidden_while_direct_queries_and_follows() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let module_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-client-rs",
            "--example",
            "activity-module",
            "--message-format=json",
        ],
    );
    let temp = unique_temp_dir("subc-sdk-activity");
    let runtime = temp.join("runtime");
    let config = temp.join("config");
    let results = temp.join("results.json");
    let trigger = temp.join("trigger");
    fs::create_dir_all(&runtime).unwrap();
    fs::create_dir_all(config.join("cortexkit")).unwrap();
    fs::write(
        config.join("cortexkit").join("subc.jsonc"),
        serde_json::to_vec_pretty(&json!({
            "version":1, "modules": { MODULE: {
                "program":module_bin.to_string_lossy(), "args":[], "enabled":true, "reserved":true,
                "env": {
                    "SUBC_ACTIVITY_RESULTS": results.to_string_lossy(),
                    "SUBC_ACTIVITY_TRIGGER": trigger.to_string_lossy(),
                },
            }},
        }))
        .unwrap(),
    )
    .unwrap();
    let mut daemon = spawn_daemon(&daemon_bin, &runtime, &config);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let result: Value = serde_json::from_str(&wait_for_file(&results).await).unwrap();
    assert_eq!(
        result["query_code"], "activity_read_forbidden",
        "query helper must attest the supervised launch identity"
    );
    assert_eq!(
        result["subscribe_code"], "activity_read_forbidden",
        "subscribe helper must attest the supervised launch identity"
    );

    let direct = SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let page = direct.query_activity(query()).await.unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].seq, result["seq"].as_u64().unwrap());
    assert_eq!(page.entries[0].summary, "initial");
    assert_eq!(
        page.entries[0].actor.principal,
        Principal::Reserved {
            module_id: MODULE.into()
        }
    );
    assert!(page.head.seq >= page.entries[0].seq);
    let mut feed = direct
        .subscribe_activity(Some(page.head.clone()))
        .await
        .unwrap();
    fs::write(&trigger, b"append").unwrap();
    let entry = timeout(EVENT_TIMEOUT, feed.next())
        .await
        .expect("Direct must receive live activity")
        .unwrap()
        .unwrap();
    assert_eq!(entry.summary, "live");
    assert_eq!(entry.module, MODULE);
    assert!(entry.seq > page.head.seq);
    feed.unsubscribe().unwrap();
    assert!(feed.next().await.unwrap().is_none());

    let mut refused = direct
        .subscribe_activity(Some(ActivityCursor {
            daemon_incarnation: "previous-daemon".into(),
            seq: page.head.seq,
        }))
        .await
        .unwrap();
    let error = timeout(EVENT_TIMEOUT, refused.next())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code(), Some("activity_cursor_incarnation_mismatch"));
    assert!(matches!(
        error,
        ActivityStreamError::Refused {
            reason: ActivityRefusal::CursorIncarnationMismatch,
            ..
        }
    ));
    direct.close().await;
    daemon.kill_and_wait();
}
