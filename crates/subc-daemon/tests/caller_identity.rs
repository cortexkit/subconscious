#![cfg(any(target_os = "macos", target_os = "linux", windows))]

use std::{net::SocketAddr, path::Path, time::Duration};
use subc_daemon::caller_identity::CallerIdentity;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    process::Command,
};

async fn assert_connecting_process_is_identified(address: SocketAddr) {
    let listener = TcpListener::bind(address).await.expect("loopback listener");
    let helper_path = Path::new(env!("CARGO_BIN_EXE_ckdev-caller-identity-helper"));
    let mut helper = Command::new(helper_path)
        .arg(listener.local_addr().unwrap().to_string())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn connecting helper");
    let helper_pid = helper.id().expect("helper pid");
    let (mut accepted, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("helper connected before deadline")
        .expect("accepted helper");
    let mut ready = [0];
    tokio::time::timeout(Duration::from_secs(10), accepted.read_exact(&mut ready))
        .await
        .expect("helper ready before deadline")
        .expect("helper holds its socket");
    assert_eq!(ready, [1]);

    let caller = CallerIdentity::lookup(&accepted);
    assert_eq!(caller.pid(), Some(helper_pid), "{}", caller.description());
    assert_ne!(caller.pid(), Some(std::process::id()), "not the listener");
    let executable = std::fs::canonicalize(caller.executable().expect("peer executable"))
        .expect("canonical peer executable");
    assert_eq!(executable, std::fs::canonicalize(helper_path).unwrap());
    assert_ne!(
        executable,
        std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap()
    );

    accepted.write_all(&[1]).await.expect("release helper");
    let status = tokio::time::timeout(Duration::from_secs(10), helper.wait())
        .await
        .expect("helper exit before deadline")
        .expect("reap helper");
    assert!(status.success(), "helper exited with {status}");
}

#[tokio::test]
async fn accepted_ipv4_connection_identifies_connecting_helper_not_listener() {
    assert_connecting_process_is_identified("127.0.0.1:0".parse().unwrap()).await;
}

#[tokio::test]
async fn accepted_ipv6_connection_identifies_connecting_helper_not_listener() {
    assert_connecting_process_is_identified("[::1]:0".parse().unwrap()).await;
}
