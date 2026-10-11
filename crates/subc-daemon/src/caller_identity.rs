//! Caller descriptions from daemon-owned connection endpoints, never client
//! claims. A missing OS lookup is an ordinary unidentified caller, not a refusal.

use std::net::SocketAddr;
use std::path::Path;
use subc_os::socket_peer::{loopback_socket_peer, SocketPeer};
use tokio::net::TcpStream;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallerIdentity {
    resolved: Option<SocketPeer>,
}

impl CallerIdentity {
    /// Resolve the connecting process from an accepted stream. This best-effort
    /// OS lookup is blocking and supplies descriptive information only.
    pub fn lookup(accepted: &TcpStream) -> Self {
        Self::lookup_with(accepted, loopback_socket_peer)
    }

    /// Resolve an endpoint pair captured from the daemon's accepted socket.
    /// Like `lookup`, this walks OS process tables and must run off async workers.
    pub(crate) fn lookup_endpoints(local: SocketAddr, peer: SocketAddr) -> Self {
        Self::from_lookup_result(loopback_socket_peer(local, peer))
    }

    fn lookup_with(
        accepted: &TcpStream,
        lookup: impl FnOnce(SocketAddr, SocketAddr) -> Option<SocketPeer>,
    ) -> Self {
        Self::from_lookup_result(
            accepted
                .local_addr()
                .ok()
                .zip(accepted.peer_addr().ok())
                .and_then(|(local, peer)| lookup(local, peer)),
        )
    }

    fn from_lookup_result(resolved: Option<SocketPeer>) -> Self {
        Self { resolved }
    }

    pub fn pid(&self) -> Option<u32> {
        self.resolved.as_ref().map(|caller| caller.pid)
    }

    /// The executable key for caller cooldowns, or `None` when the connection
    /// must be used as the fallback key instead.
    pub fn executable(&self) -> Option<&Path> {
        self.resolved
            .as_ref()
            .map(|caller| caller.executable.as_path())
    }

    /// An unbounded description. Prompt builders must still sanitize and limit
    /// the executable path to their own text budget.
    pub fn description(&self) -> String {
        match &self.resolved {
            Some(caller) => format!("pid {} ({})", caller.pid, caller.executable.display()),
            None => "an unidentified local process".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn forced_lookup_failure_is_an_ordinary_unidentified_caller() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (accepted, _) = listener.accept().await.unwrap();
        let caller = CallerIdentity::lookup_with(&accepted, |local, peer| {
            assert_eq!(local, listener.local_addr().unwrap());
            assert_eq!(peer, client.local_addr().unwrap());
            None
        });
        assert_eq!(caller.description(), "an unidentified local process");
        assert_eq!(caller.pid(), None);
        assert_eq!(caller.executable(), None);
        assert_eq!(caller, CallerIdentity::default());
    }

    #[test]
    fn resolved_description_keeps_pid_and_executable() {
        let caller = CallerIdentity::from_lookup_result(Some(SocketPeer {
            pid: 4121,
            executable: PathBuf::from("/bin/helper"),
        }));
        assert_eq!(caller.pid(), Some(4121));
        assert_eq!(caller.executable(), Some(Path::new("/bin/helper")));
        assert_eq!(caller.description(), "pid 4121 (/bin/helper)");
    }
}
