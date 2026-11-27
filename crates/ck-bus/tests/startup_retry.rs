#[allow(dead_code)]
mod harness;

use std::{fs, path::Path, time::Duration};

use cortexkit_bus_testkit::{nats_server_bin, new_machine_id, skip_or_fail};
use harness::{
    bus::{BusServer, TrustChain, LOOPBACK},
    sentinel::{is_up, wait_health},
    signer::run::{ClaustrumSide, RunOptions, SignerRun},
};
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinSet,
    time::timeout,
};

const BOOT_LIMIT: Duration = Duration::from_secs(60);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ck_bus_retries_first_nats_connection_until_broker_answers() {
    let bin = match nats_server_bin() {
        Ok((bin, _)) => bin,
        Err(error) => {
            skip_or_fail(error);
            return;
        }
    };
    let _gate = harness::acceptance_gate().await;
    let root = SignerRun::tree();
    let trust = TrustChain::generate();
    let operator_jwt = root.join("operator.jwt");
    fs::write(&operator_jwt, trust.operator_jwt()).unwrap();
    let nats_dir = root.join("nats");

    // Keep the same port bound from the rejected first connection through the healthy
    // probe. The relay lets nats-server choose its own port when it finally starts.
    let listener = TcpListener::bind((LOOPBACK, 0)).await.unwrap();
    let url = format!("nats://{}", listener.local_addr().unwrap());
    let run = SignerRun::start_with(
        root,
        Path::new(env!("CARGO_BIN_EXE_ck-bus")),
        ClaustrumSide::Signer(trust.signer.clone()),
        RunOptions {
            ckbus_env: vec![
                ("CKBUS_NATS_URL".into(), url.clone()),
                (
                    "CKBUS_OPERATOR_JWT".into(),
                    operator_jwt.display().to_string(),
                ),
                ("CKBUS_SYSTEM_ACCOUNT".into(), trust.system_account.clone()),
            ],
            machine_id: Some(new_machine_id()),
        },
    )
    .await;
    let pid = run.supervised_pid("ckbus").await;
    let (first, _) = timeout(BOOT_LIMIT, listener.accept())
        .await
        .expect("ck-bus must attempt its first NATS connection")
        .unwrap();
    drop(first);
    let first_connect = format!("connect as ckbus-system to {url}:");
    wait_health(
        &run.connection_file,
        BOOT_LIMIT,
        "the initial NATS connection failure",
        |status, _, metrics| {
            status == "Failing"
                && metrics["message"]
                    .as_str()
                    .is_some_and(|message| message.starts_with(&first_connect))
        },
    )
    .await;

    let server = BusServer::start(&bin, &nats_dir, &trust, LOOPBACK).await;
    let mut relay = JoinSet::new();
    relay.spawn(forward_to_nats(listener, server.port));
    wait_health(&run.connection_file, BOOT_LIMIT, "healthy", is_up).await;
    assert_eq!(run.supervised_pid("ckbus").await, pid);

    run.shutdown().await;
    relay.shutdown().await;
    server.stop().await;
}

async fn forward_to_nats(listener: TcpListener, port: u16) -> std::io::Result<()> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            incoming = listener.accept() => {
                let (mut client, _) = incoming?;
                connections.spawn(async move {
                    let mut broker = TcpStream::connect((LOOPBACK, port)).await?;
                    tokio::io::copy_bidirectional(&mut client, &mut broker).await
                });
            }
            _ = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}
