use super::{same_endpoint, SocketPeer};
use std::{fs, net::SocketAddr};

pub(super) fn lookup(local: SocketAddr, peer: SocketAddr) -> Option<SocketPeer> {
    // A dual-stack listener can accept an IPv4-mapped connection whose socket
    // lives in tcp6. Search both tables and compare normalized endpoints.
    let inode = ["/proc/net/tcp", "/proc/net/tcp6"]
        .into_iter()
        .filter_map(|path| fs::read_to_string(path).ok())
        .find_map(|table| connecting_inode(&table, local, peer))?;
    let socket_link = format!("socket:[{inode}]");
    for process in fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = process
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
            .filter(|pid| *pid > 0)
        else {
            continue;
        };
        let Ok(fds) = fs::read_dir(process.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if fs::read_link(fd.path()).ok().as_deref() == Some(std::path::Path::new(&socket_link))
            {
                let executable = fs::read_link(process.path().join("exe")).ok()?;
                return Some(SocketPeer { pid, executable });
            }
        }
    }
    None
}

fn connecting_inode(table: &str, local: SocketAddr, peer: SocketAddr) -> Option<u64> {
    table.lines().find_map(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        // Field 9 is the inode; only an established socket can be the peer.
        if fields.len() < 10 || fields[3] != "01" {
            return None;
        }
        let socket_local = parse_address(fields[1])?;
        let socket_remote = parse_address(fields[2])?;
        if !same_endpoint(socket_local, peer) || !same_endpoint(socket_remote, local) {
            return None;
        }
        fields[9].parse().ok().filter(|inode| *inode > 0)
    })
}

fn parse_address(field: &str) -> Option<SocketAddr> {
    let (address, port) = field.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    // procfs prints native-endian 32-bit words, including each IPv6 word.
    let bytes: Vec<u8> = address
        .as_bytes()
        .chunks_exact(8)
        .map(|word| {
            u32::from_str_radix(std::str::from_utf8(word).ok()?, 16)
                .ok()
                .map(u32::to_ne_bytes)
        })
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    match address.len() {
        8 => Some(SocketAddr::new(
            std::net::Ipv4Addr::from(<[u8; 4]>::try_from(bytes).ok()?).into(),
            port,
        )),
        32 => Some(SocketAddr::new(
            std::net::Ipv6Addr::from(<[u8; 16]>::try_from(bytes).ok()?).into(),
            port,
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_endian = "little")]
    fn proc_table_selects_connecting_inode_not_accepted_or_listening_inode() {
        let table = "sl local_address rem_address st tx_queue rx_queue tr tm retr uid timeout inode
0: 0100007F:1F90 00000000:0000 0A 0:0 00:0 0 501 0 111
1: 0100007F:1F90 0100007F:C001 01 0:0 00:0 0 501 0 222
2: 0100007F:C001 0100007F:1F90 01 0:0 00:0 0 501 0 333";
        assert_eq!(
            connecting_inode(
                table,
                "127.0.0.1:8080".parse().unwrap(),
                "127.0.0.1:49153".parse().unwrap()
            ),
            Some(333)
        );
    }

    #[test]
    #[cfg(target_endian = "little")]
    fn proc_addresses_decode_ipv6_and_ipv4_mapped_words() {
        assert_eq!(
            parse_address("00000000000000000000000001000000:1F90"),
            Some("[::1]:8080".parse().unwrap())
        );
        assert_eq!(
            parse_address("0000000000000000FFFF00000100007F:C001"),
            Some("[::ffff:127.0.0.1]:49153".parse().unwrap())
        );
    }

    #[test]
    fn malformed_proc_addresses_and_rows_are_unavailable() {
        for field in ["", "garbage:1234", "0100007F:FFFFF", "000000000:1F90"] {
            assert_eq!(parse_address(field), None, "{field}");
        }
        assert_eq!(
            connecting_inode(
                "0: broken row",
                "127.0.0.1:8080".parse().unwrap(),
                "127.0.0.1:49153".parse().unwrap()
            ),
            None
        );
    }
}
