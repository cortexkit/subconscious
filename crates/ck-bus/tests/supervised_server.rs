#[allow(dead_code)]
mod harness;
#[cfg(unix)]
#[path = "support/mod.rs"]
mod support;

#[cfg(unix)]
use harness::{control, daemon::AcceptanceRun, data_home};
#[cfg(unix)]
use std::{path::Path, time::Instant};
#[cfg(unix)]
use subc_control::{
    ClientControlRequest as Request, ClientControlResponse as Response, ModuleProtocol,
};

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a1_supervised_server_lifecycle() {
    let started = Instant::now();
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    support::version_floor();
    support::standin_preconditions();
    let operator = data_home::operator_module_dir();
    let before = data_home::fingerprint(operator.as_deref());
    let run = AcceptanceRun::start(Path::new(env!("CARGO_BIN_EXE_ck-bus"))).await;
    if let Some(dir) = &operator {
        assert!(
            !dir.starts_with(run.root.path()),
            "operator data home must be outside fixture"
        );
    }

    support::enable(&run, support::SERVER).await;
    let server = support::wait_running(&run, support::SERVER).await;
    support::protocol(&server, ModuleProtocol::None);
    assert!(server.live, "real broker must be alive before teardown");
    let _ = support::pid(&run, support::SERVER).await;
    let budget = support::budget(&server);
    let prior = support::terminals(&run, support::SERVER).await.len();
    assert_eq!(
        prior, 0,
        "real server must have no earlier terminal record before restart"
    );
    let before_restart = support::now_ms();
    let response = control::response(
        &run.connection_file,
        Request::SupervisorRestart {
            module_id: support::SERVER.into(),
            drain_timeout_ms: None,
        },
    )
    .await;
    assert!(
        matches!(response, Response::SupervisorAck { applied: true, .. }),
        "restart must apply: {response:?}"
    );
    let restarted = support::new_terminal(&run, support::SERVER, prior).await;
    clean_inside(&restarted, before_restart, budget, "restart");
    support::wait_running(&run, support::SERVER).await;
    let before_teardown = support::now_ms();
    let response = control::response(
        &run.connection_file,
        Request::SupervisorSetEnabled {
            module_id: support::SERVER.into(),
            enabled: false,
        },
    )
    .await;
    assert!(
        matches!(response, Response::SupervisorAck { applied: true, .. }),
        "teardown must apply: {response:?}"
    );
    let terminal = support::new_terminal(&run, support::SERVER, prior + 1).await;
    clean_inside(&terminal, before_teardown, budget, "teardown");

    // Three stand-ins, none of which ever registers, so no drain can reach any
    // of them over a connection and the supervisor asks each by SIGTERM:
    //
    // * `standin-none` and `standin-default-protocol` are one script that exits
    //   0 on SIGTERM, declared `protocol: "none"` and subc-by-omission. Both
    //   must stop cleanly inside the budget: a subc module that has not (or
    //   never) registered was told nothing either, and waiting out the budget
    //   for it would only put a SIGKILL behind a delay.
    // * `standin-ignores-term` ignores SIGTERM. It is the negative control: it
    //   must be SIGKILLed at or after its budget. The two clean arms can only
    //   catch a daemon that kills at the deadline if the harness can see such
    //   a kill at all; this arm proves that it can.
    for (id, expected, clean) in [
        ("standin-none", ModuleProtocol::None, true),
        ("standin-default-protocol", ModuleProtocol::Subc, true),
        ("standin-ignores-term", ModuleProtocol::None, false),
    ] {
        support::enable(&run, id).await;
        let entry = support::wait_running(&run, id).await;
        support::protocol(&entry, expected);
        if expected == ModuleProtocol::Subc {
            assert!(
                !entry.live && entry.last_probe_ms.is_none(),
                "unregistered stand-in must run without registration or probe: {entry:?}"
            );
        }
        if !clean {
            // The control's SIGTERM trap must be in place before teardown, or
            // the signal's default disposition ends it and the kill this arm
            // exists to observe never happens.
            support::wait_for_file(&run.root.join(format!("run/{id}.ready"))).await;
        }
        let budget = support::budget(&entry);
        let prior = support::terminals(&run, id).await.len();
        assert_eq!(
            prior, 0,
            "{id} must have no earlier terminal record before teardown"
        );
        let start = support::now_ms();
        let response = control::response(
            &run.connection_file,
            Request::SupervisorSetEnabled {
                module_id: id.into(),
                enabled: false,
            },
        )
        .await;
        assert!(
            matches!(response, Response::SupervisorAck { applied: true, .. }),
            "{id} teardown must apply: {response:?}"
        );
        let terminal = support::new_terminal(&run, id, prior).await;
        let elapsed = support::elapsed(&terminal, start);
        eprintln!("A1 {id} teardown elapsed={elapsed}ms budget={budget}ms record={terminal:?}");
        if clean {
            assert!(elapsed < budget, "{id} SIGTERM must finish inside budget");
            assert!(
                terminal.exit_code == Some(0) || terminal.exit_signal == Some(15),
                "{id} must stop cleanly: {terminal:?}"
            );
            assert_ne!(terminal.exit_signal, Some(9), "{id} must not be SIGKILLed");
        } else {
            assert!(
                elapsed >= budget,
                "{id} ignores SIGTERM, so it must outlive its budget"
            );
            assert_eq!(
                terminal.exit_signal,
                Some(9),
                "{id} must be SIGKILLed once its budget runs out: {terminal:?}"
            );
        }
    }
    run.shutdown().await;
    assert_eq!(
        data_home::fingerprint(operator.as_deref()),
        before,
        "operator data home must remain unchanged"
    );
    eprintln!("A1 lifecycle wall time: {:?}", started.elapsed());
    harness::report::RowReport::passed(harness::report::Row::SupervisedServer)
        .served_by(harness::report::ServedBy::None)
        .validate(&Default::default())
        .unwrap();
}

#[cfg(unix)]
fn clean_inside(record: &subc_control::TerminalEntry, start: u64, budget: u64, teardown: &str) {
    let elapsed = support::elapsed(record, start);
    eprintln!(
        "A1 real nats-server {teardown} elapsed={elapsed}ms budget={budget}ms record={record:?}"
    );
    assert!(
        elapsed < budget,
        "{teardown} terminal must land strictly inside effective budget"
    );
    assert!(
        record.exit_code == Some(0) || record.exit_signal == Some(15),
        "{teardown} must stop cleanly: {record:?}"
    );
    assert_ne!(record.exit_signal, Some(9), "{teardown} must not SIGKILL");
}

#[cfg(not(unix))]
#[test]
fn a1_supervised_server_lifecycle() {
    let report = harness::report::RowReport::skipped(
        harness::report::Row::SupervisedServer,
        "a1-signal-unix-only",
        "non-unix host",
    )
    .served_by(harness::report::ServedBy::None);
    report.validate(&Default::default()).unwrap();
    eprintln!("a1-signal-unix-only: non-unix host");
}
