use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use subc_transport::{authenticate_client, ConnectionInfo, Endpoint, SCHEMA_VERSION};

/// `subc-client-rs` gives the whole handshake this long before the module
/// exits with `Timeout { stage: ServerProof }`.
const MODULE_AUTH_DEADLINE: Duration = Duration::from_secs(2);
/// How long the replacement's admission write is held: longer than the
/// module's deadline, standing in for the delay a busy disk can add
/// across the two syncs in that write.
const HELD_RECORD_WRITE: Duration = Duration::from_secs(3);

/// A module that runs `/bin/sleep` directly, not through `/bin/sh -c`.
/// On macOS every module is launched through the privacy trampoline, and
/// a module that is not a `#!` script is admitted only if its running
/// image is the configured program. With `/bin/sh -c "exec sleep 600"`
/// the shell soon replaces itself with `/bin/sleep`, which does not match
/// the configured `/bin/sh`. The image is read only after the spawn's
/// live-children record write, and here that write lasted long enough for
/// the shell to have exec'd every time, so each process was refused and
/// killed right after spawn, and the supervisor crash-looped it with
/// backoff until its restart budget ran out. The restart this test
/// measures would then never produce a module at all.
fn sleeping_spec() -> ModuleSpec {
    ModuleSpec {
        script: None,
        module_id: "record-write-stall".into(),
        program: "/bin/sleep".into(),
        args: vec!["600".into()],
        env: vec![],
        reserved: false,
        reserved_prefixes: vec![],
        protocol: ModuleProtocol::None,
        overlap: Default::default(),
        carry_containment: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_auth_completes_while_the_restart_waits_on_the_record_write() {
    let home = cortexkit_test_support::ScratchDir::new("record-write-stall");
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
            .with_live_children_record(home.join("live-children.json"));
    let module = supervisor.spawn(sleeping_spec()).unwrap();
    // `reported_pid` names a process only once it is confirmed as the
    // module (on macOS, once the trampoline's image check has passed), so
    // a process the daemon is about to refuse and kill never counts as
    // the running module, here or in the final wait below.
    let old_pid = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(pid) = lock_snapshot(&module.inner.snapshot)
                .unwrap()
                .reported_pid()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the first process was admitted as the module");

    // The daemon's listener, served by this runtime's workers like the
    // supervisor's own tasks.
    let key = vec![0x42; 32];
    let daemon_id = [0x24; 16];
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(crate::server::serve_listener(
        listener,
        Arc::new(crate::router::Router::with_default_self_handler()),
        crate::server::ServerAuth::with_limits(
            key.clone(),
            daemon_id,
            "test",
            crate::server::DEFAULT_AUTH_DEADLINE,
            4,
        ),
    ));

    // Hold the write that admits the replacement, once, and say when it
    // has started.
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let armed = AtomicBool::new(true);
    supervisor
        .child_roster
        .set_record_write_hook_for_test(move |live| {
            let admitting_replacement = live.values().any(|entry| entry.pid != old_pid);
            if admitting_replacement && armed.swap(false, Ordering::SeqCst) {
                let _ = held_tx.send(());
                std::thread::sleep(HELD_RECORD_WRITE);
            }
        });

    // The replacement module, as a separate process would be: its own
    // thread and runtime, so only the daemon's runtime can be stalled.
    let module_process = std::thread::spawn(move || {
        held_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the restart reached the replacement's record write");
        let conn = ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: None,
            endpoints: vec![Endpoint {
                host: "127.0.0.1".to_owned(),
                port,
            }],
            key,
            daemon_id,
            pid: std::process::id(),
            daemon_ver: "test".to_owned(),
        };
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let started = Instant::now();
                let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .unwrap();
                let result = authenticate_client(&mut stream, &conn, MODULE_AUTH_DEADLINE).await;
                (result, started.elapsed())
            })
    });

    module.restart(Some(0)).await.unwrap();
    let (auth, elapsed) = tokio::task::spawn_blocking(move || module_process.join().unwrap())
        .await
        .unwrap();
    assert!(
        auth.is_ok(),
        "the daemon must answer a module's handshake while a restart waits on the \
             live-children record write; got {auth:?} after {elapsed:?}"
    );

    tokio::time::timeout(Duration::from_secs(20), async {
        while lock_snapshot(&module.inner.snapshot)
            .unwrap()
            .reported_pid()
            .is_none_or(|pid| pid == old_pid)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the restart published its replacement");
    module.stop().await.unwrap();
    server.abort();
}
