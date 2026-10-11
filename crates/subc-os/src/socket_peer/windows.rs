#![allow(unsafe_code)]

use super::{same_endpoint, SocketPeer};
use std::{
    ffi::OsString,
    mem::size_of,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    os::windows::{
        ffi::OsStringExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::PathBuf,
};
use windows_sys::Win32::{
    Foundation::ERROR_INSUFFICIENT_BUFFER,
    NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID, MIB_TCP_STATE_ESTAB,
        TCP_TABLE_OWNER_PID_ALL,
    },
    Networking::WinSock::{AF_INET, AF_INET6},
    System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    },
};

pub(super) fn lookup(local: SocketAddr, peer: SocketAddr) -> Option<SocketPeer> {
    // Try both families because a dual-stack accepted socket may describe an
    // IPv4 peer using mapped IPv6 addresses.
    let pid = ipv4_pid(local, peer).or_else(|| ipv6_pid(local, peer))?;
    let executable = executable(pid)?;
    Some(SocketPeer { pid, executable })
}

fn ipv4_pid(local: SocketAddr, peer: SocketAddr) -> Option<u32> {
    let (table, size) = tcp_table(u32::from(AF_INET))?;
    rows::<MIB_TCPROW_OWNER_PID>(&table, size)?
        .into_iter()
        .find_map(|row| {
            let socket_local = SocketAddr::new(
                Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes()).into(),
                network_port(row.dwLocalPort),
            );
            let socket_remote = SocketAddr::new(
                Ipv4Addr::from(row.dwRemoteAddr.to_ne_bytes()).into(),
                network_port(row.dwRemotePort),
            );
            (row.dwState == MIB_TCP_STATE_ESTAB as u32
                && same_endpoint(socket_local, peer)
                && same_endpoint(socket_remote, local)
                && row.dwOwningPid != 0)
                .then_some(row.dwOwningPid)
        })
}

fn ipv6_pid(local: SocketAddr, peer: SocketAddr) -> Option<u32> {
    let (table, size) = tcp_table(u32::from(AF_INET6))?;
    rows::<MIB_TCP6ROW_OWNER_PID>(&table, size)?
        .into_iter()
        .find_map(|row| {
            let socket_local = SocketAddr::new(
                Ipv6Addr::from(row.ucLocalAddr).into(),
                network_port(row.dwLocalPort),
            );
            let socket_remote = SocketAddr::new(
                Ipv6Addr::from(row.ucRemoteAddr).into(),
                network_port(row.dwRemotePort),
            );
            (row.dwState == MIB_TCP_STATE_ESTAB as u32
                && same_endpoint(socket_local, peer)
                && same_endpoint(socket_remote, local)
                && row.dwOwningPid != 0)
                .then_some(row.dwOwningPid)
        })
}

fn network_port(port: u32) -> u16 {
    u16::from_be(port as u16)
}

fn tcp_table(family: u32) -> Option<(Vec<u32>, usize)> {
    let mut size = 0u32;
    // SAFETY: a null buffer with a valid size pointer queries the required
    // length. The owner-PID table class supports both requested families.
    let status = unsafe {
        GetExtendedTcpTable(
            std::ptr::null_mut(),
            &mut size,
            0,
            family,
            TCP_TABLE_OWNER_PID_ALL,
            0,
        )
    };
    if status != ERROR_INSUFFICIENT_BUFFER {
        return None;
    }
    for _ in 0..4 {
        if !(4..=16 * 1024 * 1024).contains(&size) {
            return None;
        }
        // u32 storage provides the native TCP table's four-byte alignment.
        let mut buffer = vec![0u32; (size as usize).div_ceil(size_of::<u32>())];
        let capacity = buffer.len() * size_of::<u32>();
        size = capacity as u32;
        // SAFETY: buffer is writable for the supplied byte capacity and the
        // size pointer is live. No pointers into the buffer escape this call.
        let status = unsafe {
            GetExtendedTcpTable(
                buffer.as_mut_ptr().cast(),
                &mut size,
                0,
                family,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            )
        };
        if status == 0 && size as usize <= capacity {
            return Some((buffer, size as usize));
        }
        if status != ERROR_INSUFFICIENT_BUFFER {
            return None;
        }
        // The table can grow between sizing and reading; retry with the new
        // size, bounded so continuous socket churn cannot hold a request.
    }
    None
}

// Only these two C table rows are admitted: they contain integers and byte
// arrays, so every bit pattern is valid and neither contains pointers.
trait TableRow: Copy {}
impl TableRow for MIB_TCPROW_OWNER_PID {}
impl TableRow for MIB_TCP6ROW_OWNER_PID {}

fn rows<T: TableRow>(buffer: &[u32], size: usize) -> Option<Vec<T>> {
    if size < 4 || size > std::mem::size_of_val(buffer) {
        return None;
    }
    let count = *buffer.first()? as usize;
    if count > (size - 4) / size_of::<T>() {
        return None;
    }
    Some(
        (0..count)
            .map(|index| {
                // SAFETY: the validated count keeps this complete row within the
                // initialized table. T is one of the two pointer-free native rows;
                // read_unaligned also avoids imposing a Rust alignment requirement.
                unsafe {
                    buffer
                        .as_ptr()
                        .cast::<u8>()
                        .add(4 + index * size_of::<T>())
                        .cast::<T>()
                        .read_unaligned()
                }
            })
            .collect(),
    )
}

fn executable(pid: u32) -> Option<PathBuf> {
    // SAFETY: OpenProcess takes scalar arguments and returns an owned handle
    // or null. Request only the rights needed to describe the image.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    // SAFETY: the successful OpenProcess handle is owned here exactly once;
    // OwnedHandle closes it on every return path.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut path = vec![0u16; 32768];
    let mut length = path.len() as u32;
    // SAFETY: the retained handle is live, and path/length are writable for
    // the declared UTF-16 capacity. The returned length excludes the NUL.
    let success = unsafe {
        QueryFullProcessImageNameW(handle.as_raw_handle(), 0, path.as_mut_ptr(), &mut length)
    };
    if success == 0 || length == 0 || length as usize > path.len() {
        return None;
    }
    Some(PathBuf::from(OsString::from_wide(&path[..length as usize])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_owner_table_bounds_reject_truncated_rows() {
        assert!(rows::<MIB_TCPROW_OWNER_PID>(&[1, 0], 8).is_none());
        assert!(rows::<MIB_TCP6ROW_OWNER_PID>(&[1, 0], 8).is_none());
        assert!(rows::<MIB_TCPROW_OWNER_PID>(&[0], 4).unwrap().is_empty());
    }

    #[test]
    fn tcp_table_ports_are_network_order_not_host_order() {
        assert_eq!(network_port(u32::from(8080u16.to_be())), 8080);
    }
}
