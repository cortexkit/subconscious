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
