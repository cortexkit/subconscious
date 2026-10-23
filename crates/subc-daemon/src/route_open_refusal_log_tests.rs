// The `route.open refused` line must name its caller on its own. Connection
// ids restart with the daemon, so without the principal, scope and harness a
// refusal read after a restart can be pinned on whichever module held that
// connection id before it. These tests read the real formatter's output, so
// quoting and escaping are checked as an operator's log shows them.

use std::future::Future;

use crate::router::test_log::{captured_logs, log_capture};

/// A route.open on a fresh client connection, with every caller field the
/// refused line reports under the test's control.
fn refusal_open_frame(
    rig: &mut Rig,
    opener: Option<(&str, String)>,
    target: &str,
    scope: Option<ScopeSelector>,
    harness: &str,
) -> (
    RouteCtx,
    mpsc::Receiver<crate::router::OutboundFrame>,
    Frame,
    ScratchDir,
) {
    rig.next_connection += 1;
    let (ctx, rx) = wide_ctx(rig.next_connection);
    let root = unique_project_root("refused-line");
    let body = serde_json::to_vec(&ClientControlRequest::RouteOpen {
        target: RouteTarget::ToolProvider {
            module_id: target.to_string(),
        },
        identity: BindIdentity::new(
            root.path().to_path_buf(),
            harness.to_string(),
            "session".to_string(),
        ),
        consumer_identity: opener.map(|(module_id, launch_nonce)| ConsumerIdentity {
            module_id: module_id.to_string(),
            launch_nonce,
        }),
        consumer_capabilities: None,
        role_versions: None,
        admission_facts: None,
        scope,
    })
    .unwrap();
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        rig.next_connection,
        body,
    )
    .unwrap();
    (ctx, rx, frame, root)
}

fn refused_line(logs: &str) -> &str {
    let refused = logs
        .lines()
        .filter(|line| line.contains("route.open refused"))
        .collect::<Vec<_>>();
    assert_eq!(refused.len(), 1, "{logs}");
    refused[0]
}

/// Refused before scope admission (the target is not registered): the line
/// names the attested principal, the scope as requested, and the harness.
#[tokio::test]
async fn refused_line_names_principal_requested_scope_and_harness() {
    let mut rig = rig().await;
    let scope = rig.selector("scope-reference-123456789", Some(1));
    let (ctx, _rx, frame, _root) =
        refusal_open_frame(&mut rig, Some((AFT, nonce(AFT))), "nobody", Some(scope), "unit");
    let (output, subscriber) = log_capture(tracing::Level::INFO);
    let replies = rig.handler.handle_control_frame(&ctx, frame).await.unwrap();
    drop(subscriber);
    assert_eq!(parse_error(&replies[0])["code"], "unknown_module");
    let logs = captured_logs(&output);
    let line = refused_line(&logs);
    for field in [
        "principal=reserved:aft",
        r#"scope=requested/"reserved:prefrontal-core"/"scope-reference-123456789"@1"#,
        r#"harness="unit""#,
    ] {
        assert!(line.contains(field), "missing {field} in {line}");
    }
}

/// An unscoped open from a direct client says so in both fields.
#[tokio::test]
async fn refused_line_names_a_direct_unscoped_caller() {
    let mut rig = rig().await;
    let (ctx, _rx, frame, _root) = refusal_open_frame(&mut rig, None, "nobody", None, "cli");
    let (output, subscriber) = log_capture(tracing::Level::INFO);
    rig.handler.handle_control_frame(&ctx, frame).await.unwrap();
    drop(subscriber);
    let logs = captured_logs(&output);
    let line = refused_line(&logs);
    for field in ["principal=direct", "scope=none", r#"harness="cli""#] {
        assert!(line.contains(field), "missing {field} in {line}");
    }
}

/// A consumer identity whose launch nonce does not verify is logged as
/// `unattested`: the module id it claims is the caller's word, and printing
/// it as the principal would let any client sign a refusal with another
/// module's name.
#[tokio::test]
async fn refused_line_never_prints_an_unverified_claim_as_the_principal() {
    let mut rig = rig().await;
    let (ctx, _rx, frame, _root) = refusal_open_frame(
        &mut rig,
        Some((AFT, "not-the-launch-nonce".to_string())),
        PLEXUS,
        None,
        "unit",
    );
    let (output, subscriber) = log_capture(tracing::Level::INFO);
    let replies = rig.handler.handle_control_frame(&ctx, frame).await.unwrap();
    drop(subscriber);
    assert_eq!(parse_error(&replies[0])["code"], "bad_consumer_identity");
    let logs = captured_logs(&output);
    let line = refused_line(&logs);
    assert!(line.contains("principal=unattested"), "{line}");
    assert!(!line.contains("reserved:aft"), "{line}");
}

/// A refusal after admission names the scope by the stamp admission issued,
/// in the same spelling as the `route.open accepted` line.
#[tokio::test(start_paused = true)]
async fn refused_line_after_admission_names_the_admitted_scope() {
    let mut rig = rig().await;
    rig.sync(vec![session(1)]).await;
    let scope = rig.selector("s", Some(1));
    let (ctx, _rx, frame, _root) =
        refusal_open_frame(&mut rig, Some((AFT, nonce(AFT))), PLEXUS, Some(scope), "unit");
    let (output, subscriber) = log_capture(tracing::Level::INFO);
    let mut open = Box::pin(rig.handler.handle_control_frame(&ctx, frame));
    // Poll through admission and the bind send; the open then waits for the
    // target's answer to the bind.
    std::future::poll_fn(|cx| {
        assert!(open.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    let (module, module_rx) = rig.modules.get_mut(PLEXUS).unwrap();
    let bind = module_rx.try_recv().expect("bind sent to target").frame;
    let rejection = Frame::build(
        FrameType::Error,
        control_flags(),
        0,
        0,
        bind.header.corr,
        serde_json::to_vec(&ErrorBody::new("not_today", "module refused route.bind")).unwrap(),
    )
    .unwrap();
    rig.handler
        .handle_control_frame(module, rejection)
        .await
        .unwrap();
    let replies = open.await.unwrap();
    assert_eq!(parse_error(&replies[0])["code"], "not_today");
    drop(subscriber);
    let logs = captured_logs(&output);
    let line = refused_line(&logs);
    for field in [
        "code=\"module_rejected\"",
        "principal=reserved:aft",
        r#"scope=head/reserved:prefrontal-core/"s"@1"#,
        r#"harness="unit""#,
    ] {
        assert!(line.contains(field), "missing {field} in {line}");
    }
}

/// The scope ref and the harness are the caller's own strings: they are
/// escaped, so a newline cannot start a forged log line, and cut to 64
/// characters.
#[tokio::test]
async fn refused_line_escapes_and_bounds_caller_text() {
    let mut rig = rig().await;
    let forged = "x\n2026-01-01T00:00:00Z INFO route.open accepted principal=reserved:claustrum";
    let scope = rig.selector(forged, Some(1));
    let long_harness = "h".repeat(100);
    let (ctx, _rx, frame, _root) =
        refusal_open_frame(&mut rig, None, "nobody", Some(scope), &long_harness);
    let (output, subscriber) = log_capture(tracing::Level::INFO);
    rig.handler.handle_control_frame(&ctx, frame).await.unwrap();
    drop(subscriber);
    let logs = captured_logs(&output);
    assert!(
        !logs.lines().any(|line| line.starts_with("2026-01-01")),
        "{logs}"
    );
    let line = refused_line(&logs);
    assert!(line.contains(r#"/"x\n2026-01-01T00:00:00Z INFO route.open"#), "{line}");
    assert!(
        line.contains(&format!("harness=\"{}…\"", "h".repeat(64))),
        "{line}"
    );
    assert!(!line.contains(&"h".repeat(65)), "{line}");
}

/// The three caller fields, as the real formatter prints them for an open
/// made by aft under the owner's scope `s` at epoch 1 with harness `unit`.
fn assert_caller_fields(line: &str, scope: &str) {
    for field in [
        "principal=reserved:aft".to_string(),
        format!("scope={scope}"),
        r#"harness="unit""#.to_string(),
    ] {
        assert!(line.contains(&field), "missing {field} in {line}");
    }
}

const REQUESTED_S: &str = r#"requested/"reserved:prefrontal-core"/"s"@1"#;
const ADMITTED_S: &str = r#"head/reserved:prefrontal-core/"s"@1"#;

/// A configured module that is stopped and has no registration, the state a
/// module is in while an operator has it stopped. Its refusal is logged by
/// its own `info!` (reason `supervised_not_registered`), not the shared one.
#[tokio::test]
async fn supervised_absent_refused_line_names_the_caller() {
    let mut rig = rig().await;
    // Configured with enabled: false, so the supervisor lists the module
    // without spawning a process and it never registers.
    rig._supervisor
        .supervise_configured(
            ModuleSpec {
                module_id: "stopped".to_string(),
                program: PathBuf::from("/nonexistent/stopped"),
                args: Vec::new(),
                env: Vec::new(),
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::Subc,
                overlap: Default::default(),
            },
            false,
        )
        .unwrap();
    let scope = rig.selector("s", Some(1));
    let (ctx, _rx, frame, _root) =
        refusal_open_frame(&mut rig, Some((AFT, nonce(AFT))), "stopped", Some(scope), "unit");
    let (output, subscriber) = log_capture(tracing::Level::INFO);
    let replies = rig.handler.handle_control_frame(&ctx, frame).await.unwrap();
    drop(subscriber);
    assert_eq!(parse_error(&replies[0])["code"], "target_unavailable");
    let logs = captured_logs(&output);
    let line = refused_line(&logs);
    assert!(line.contains(r#"reason="supervised_not_registered""#), "{line}");
    assert_caller_fields(line, REQUESTED_S);
}

/// A registered module that has declared itself not ready is refused with
/// `module_warming` from its own `info!` (reason `declared_not_ready`).
#[tokio::test]
async fn declared_not_ready_refused_line_names_the_caller() {
    let mut rig = rig().await;
    let provides = rig
        .handler
        .registry
        .get_module(PLEXUS)
        .unwrap()
        .unwrap()
        .manifest
        .provides;
    let plexus_connection = rig.modules[PLEXUS].0.connection_id;
    rig.handler
        .registry
        .replace_catalog_for_connection(plexus_connection, provides, None, Some(false))
        .unwrap()
        .expect("plexus is registered");
    let scope = rig.selector("s", Some(1));
    let (ctx, _rx, frame, _root) =
        refusal_open_frame(&mut rig, Some((AFT, nonce(AFT))), PLEXUS, Some(scope), "unit");
    let (output, subscriber) = log_capture(tracing::Level::INFO);
    let replies = rig.handler.handle_control_frame(&ctx, frame).await.unwrap();
    drop(subscriber);
    assert_eq!(parse_error(&replies[0])["code"], "module_warming");
    let logs = captured_logs(&output);
    let line = refused_line(&logs);
    assert!(line.contains(r#"reason="declared_not_ready""#), "{line}");
    assert_caller_fields(line, REQUESTED_S);
}

/// A module whose bind-relay breaker is open is refused before any relay,
/// after scope admission, on a debug-level line of its own.
#[tokio::test]
async fn breaker_refused_line_names_the_caller() {
    let mut rig = rig().await;
    rig.sync(vec![session(1)]).await;
    // One timeout at threshold 1 opens the breaker for a minute.
    assert!(rig
        .handler
        .route_bind_breakers
        .record_timeout(PLEXUS, None, 1, Duration::from_secs(60))
        .is_some());
    let scope = rig.selector("s", Some(1));
    let (ctx, _rx, frame, _root) =
        refusal_open_frame(&mut rig, Some((AFT, nonce(AFT))), PLEXUS, Some(scope), "unit");
    let (output, subscriber) = log_capture(tracing::Level::DEBUG);
    let replies = rig.handler.handle_control_frame(&ctx, frame).await.unwrap();
    drop(subscriber);
    assert_eq!(parse_error(&replies[0])["code"], "module_timeout");
    let logs = captured_logs(&output);
    let line = refused_line(&logs);
    assert!(
        line.contains("route.open refused by open bind-relay breaker"),
        "{line}"
    );
    assert_caller_fields(line, ADMITTED_S);
}
