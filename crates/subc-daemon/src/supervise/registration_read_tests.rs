use super::*;
use tokio::io::AsyncReadExt;

struct ReadError(io::ErrorKind);

impl tokio::io::AsyncRead for ReadError {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Err(io::Error::from(self.0)))
    }
}

#[tokio::test]
async fn registration_retirement_interrupted_read_keeps_spawn_alive() {
    let handle = SupervisorHandle::new();
    let spawn = RegistrationSpawn::new(&handle, "interrupted-reader", SpawnRole::Plain, None);
    spawn.ready();
    let mut stream = RegistrationStream {
        stream: ReadError(io::ErrorKind::Interrupted),
        spawn: spawn.clone(),
    };
    assert_eq!(
        stream.read(&mut [0]).await.unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
    assert!(
        spawn.launch_ready(),
        "an interrupted read is not an exit observation"
    );
    stream.stream.0 = io::ErrorKind::ConnectionReset;
    assert_eq!(
        stream.read(&mut [0]).await.unwrap_err().kind(),
        io::ErrorKind::ConnectionReset
    );
    assert!(!spawn.launch_ready(), "other read errors retire the launch");
}

#[tokio::test]
async fn nonce_attestation_refuses_a_nonce_whose_recorded_launch_is_gone() {
    let handle = SupervisorHandle::new();
    // A nonce no launch was ever recorded for attests its module with no
    // record, as launch nonces did before launches kept records.
    handle.set_spawn_nonce("unrecorded", "legacy-nonce".into());
    assert!(matches!(
        handle.attest_spawned_consumer("unrecorded", "legacy-nonce"),
        NonceAttestation::Unrecorded
    ));

    handle.set_spawn_nonce("recorded", "launch-nonce".into());
    let spawn = RegistrationSpawn::new(&handle, "recorded", SpawnRole::Plain, None);
    handle.record_nonce_launch(&spawn);
    let key = spawn.authority.key();
    assert!(matches!(
        handle.attest_spawned_consumer("recorded", "launch-nonce"),
        NonceAttestation::Spawn(record) if record.key() == key
    ));
    assert!(matches!(
        handle.attest_spawned_consumer("recorded", "wrong-nonce"),
        NonceAttestation::Refused
    ));

    // The launch ends while its nonce still authorizes, and a later launch
    // of another module prunes ended launches: the nonce's record is gone,
    // so it attests nothing rather than a module with no record.
    drop(spawn);
    handle.set_spawn_nonce("other-module", "other-nonce".into());
    let other = RegistrationSpawn::new(&handle, "other-module", SpawnRole::Plain, None);
    handle.record_nonce_launch(&other);
    assert!(handle.spawned_consumer_authorized("recorded", "launch-nonce"));
    assert!(matches!(
        handle.attest_spawned_consumer("recorded", "launch-nonce"),
        NonceAttestation::Refused
    ));
}
