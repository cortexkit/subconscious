//! Best-effort process descriptions for the peer of an accepted loopback TCP
//! connection. These snapshots are not credentials: processes can exit, exec,
//! or share a socket while a lookup is running. macOS libproc and Linux procfs
//! enumerate socket holders; if more than one visible process holds the peer
//! socket (for example after fork), the caller is ambiguous and unresolved.
//! Windows' TCP owner table reports one owning PID per connection, not every
//! process with an inherited or duplicated socket handle, so it cannot detect
//! that kind of sharing. None of these OS snapshots proves exclusive ownership.

use std::{net::SocketAddr, path::PathBuf};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

/// The process holding the connecting end of a loopback TCP connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SocketPeer {
    pub pid: u32,
    pub executable: PathBuf,
}

/// Resolve the connecting process, given the accepted stream's local and peer
/// addresses, in that order. The lookup matches both endpoints in reverse:
/// the owning socket's local address is `peer`, not the listener's address.
///
/// Returns `None` on unsupported platforms, non-loopback connections, lookup
/// races, multiple visible holders, or insufficient OS visibility. This is a
/// blocking, diagnostic lookup, not an authorization check; failure must not by
/// itself refuse a request.
pub fn loopback_socket_peer(local: SocketAddr, peer: SocketAddr) -> Option<SocketPeer> {
    lookup_with(local, peer, platform_lookup)
}

fn lookup_with(
    local: SocketAddr,
    peer: SocketAddr,
    lookup: impl FnOnce(SocketAddr, SocketAddr) -> Option<SocketPeer>,
) -> Option<SocketPeer> {
    if !normalized(local).ip().is_loopback()
        || !normalized(peer).ip().is_loopback()
        || same_endpoint(local, peer)
    {
        return None;
    }
    lookup(local, peer)
}

fn platform_lookup(local: SocketAddr, peer: SocketAddr) -> Option<SocketPeer> {
    #[cfg(target_os = "linux")]
    return linux::lookup(local, peer);
    #[cfg(target_os = "macos")]
    return macos::lookup(local, peer);
    #[cfg(windows)]
    return windows::lookup(local, peer);
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    None
}

fn normalized(address: SocketAddr) -> SocketAddr {
    match address {
        SocketAddr::V6(v6) => v6
            .ip()
            .to_ipv4_mapped()
            .map(|ip| SocketAddr::new(ip.into(), v6.port()))
            .unwrap_or(address),
        _ => address,
    }
}

fn same_endpoint(left: SocketAddr, right: SocketAddr) -> bool {
    let left = normalized(left);
    let right = normalized(right);
    left.ip() == right.ip() && left.port() == right.port()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_loopback_and_identical_endpoints_are_not_looked_up() {
        let local = "127.0.0.1:1234".parse().unwrap();
        let must_not_lookup = |_, _| panic!("invalid endpoints reached OS lookup");
        assert_eq!(lookup_with(local, local, must_not_lookup), None);
        assert_eq!(
            lookup_with(local, "192.0.2.1:5678".parse().unwrap(), must_not_lookup),
            None
        );
        assert_eq!(
            lookup_with("192.0.2.1:1234".parse().unwrap(), local, must_not_lookup),
            None
        );
    }

    #[test]
    fn valid_loopback_endpoints_reach_lookup_in_accepted_order() {
        let local = "127.0.0.1:1234".parse().unwrap();
        let peer = "127.0.0.1:5678".parse().unwrap();
        let caller = SocketPeer {
            pid: 42,
            executable: PathBuf::from("/bin/helper"),
        };
        assert_eq!(
            lookup_with(local, peer, |seen_local, seen_peer| {
                assert_eq!(seen_local, local);
                assert_eq!(seen_peer, peer);
                Some(caller.clone())
            }),
            Some(caller)
        );
    }

    #[test]
    fn ipv4_mapped_endpoints_match_ipv4_but_not_other_ports() {
        assert!(same_endpoint(
            "[::ffff:127.0.0.1]:1234".parse().unwrap(),
            "127.0.0.1:1234".parse().unwrap()
        ));
        assert!(!same_endpoint(
            "[::ffff:127.0.0.1]:1234".parse().unwrap(),
            "127.0.0.1:5678".parse().unwrap()
        ));
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod shared_tests;
