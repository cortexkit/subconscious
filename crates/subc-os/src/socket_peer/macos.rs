#![allow(unsafe_code)]

use super::{same_endpoint, SocketPeer};
use std::{
    ffi::OsString,
    mem::size_of,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    os::unix::ffi::OsStringExt,
    path::PathBuf,
};

// Darwin's proc_info.h defines these libproc flavors and descriptor tags.
const PROC_PIDLISTFDS: i32 = 1;
const PROC_PIDFDSOCKETINFO: i32 = 3;
const PROX_FDTYPE_SOCKET: u32 = 2;
const SOCKINFO_TCP: u32 = 2;
const TCPS_ESTABLISHED: u32 = 4;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcFdInfo {
    fd: i32,
    fd_type: u32,
}

// libc exposes proc_pidfdinfo but not socket_fdinfo. Its stable LP64 prefix
// is defined in Apple's bsd/sys/proc_info.h: the embedded proc_fileinfo
// (pfi, not the descriptor-list ProcFdInfo above) is 24 bytes, then psi;
// soi_family is at 184, soi_kind at 256, and pri_tcp at 264. The trailing
// protocol union grows as Darwin adds socket kinds, so reserve more than the
// current struct size and validate the returned prefix before decoding it.
// https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/proc_info.h
#[repr(C, align(8))]
struct SocketInfo([u8; 1024]);

pub(super) fn lookup(local: SocketAddr, peer: SocketAddr) -> Option<SocketPeer> {
    let mut holder = None;
    for pid in pids()? {
        if pid <= 0 {
            continue;
        }
        let Some(fds) = descriptors(pid) else {
            continue;
        };
        for fd in fds {
            if fd.fd_type != PROX_FDTYPE_SOCKET {
                continue;
            }
            let mut info = SocketInfo([0; 1024]);
            // SAFETY: info is an eight-byte-aligned, writable buffer of the
            // supplied size. pid/fd are scalars; failures leave no borrowed data.
            let count = unsafe {
                libc::proc_pidfdinfo(
                    pid,
                    fd.fd,
                    PROC_PIDFDSOCKETINFO,
                    info.0.as_mut_ptr().cast(),
                    info.0.len() as i32,
                )
            };
            if count < 348 || count as usize > info.0.len() {
                continue;
            }
            if matches_connecting_socket(&info.0, local, peer) {
                // Count processes, not descriptors: dup within a process does
                // not make its identity ambiguous, but inheritance by a fork does.
                if holder.is_some() {
                    return None;
                }
                holder = Some(pid);
                break;
            }
        }
    }
    let pid = holder?;
    Some(SocketPeer {
        pid: pid as u32,
        executable: executable(pid)?,
    })
}

fn pids() -> Option<Vec<i32>> {
    // SAFETY: null/zero queries the number of processes without writing data.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return None;
    }
    let mut capacity = count as usize + 32;
    for _ in 0..4 {
        if capacity > 1_000_000 {
            return None;
        }
        let mut pids = vec![0i32; capacity];
        // SAFETY: pids is initialized, correctly aligned i32 storage, writable
        // for the supplied byte count. The API returns a count of PIDs.
        let count = unsafe {
            libc::proc_listallpids(
                pids.as_mut_ptr().cast(),
                (pids.len() * size_of::<i32>()) as i32,
            )
        };
        if count <= 0 || count as usize > pids.len() {
            return None;
        }
        if (count as usize) < pids.len() {
            pids.truncate(count as usize);
            return Some(pids);
        }
        capacity *= 2;
    }
    None
}

fn descriptors(pid: i32) -> Option<Vec<ProcFdInfo>> {
    // SAFETY: this flavor supports null/zero to query the required byte count.
    let bytes = unsafe { libc::proc_pidinfo(pid, PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return None;
    }
    let mut capacity = (bytes as usize).div_ceil(size_of::<ProcFdInfo>()) + 32;
    for _ in 0..4 {
        if capacity > 1_000_000 {
            return None;
        }
        let mut fds = vec![ProcFdInfo::default(); capacity];
        // SAFETY: ProcFdInfo matches the two 32-bit fields in proc_fdinfo. The
        // initialized storage is writable for the supplied byte count, and any
        // returned integer bit patterns are valid Rust values.
        let bytes = unsafe {
            libc::proc_pidinfo(
                pid,
                PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr().cast(),
                (fds.len() * size_of::<ProcFdInfo>()) as i32,
            )
        };
        if bytes <= 0
            || bytes as usize > fds.len() * size_of::<ProcFdInfo>()
            || !(bytes as usize).is_multiple_of(size_of::<ProcFdInfo>())
        {
            return None;
        }
        let count = bytes as usize / size_of::<ProcFdInfo>();
        if count < fds.len() {
            fds.truncate(count);
            return Some(fds);
        }
        capacity *= 2;
    }
    None
}

fn matches_connecting_socket(info: &[u8; 1024], local: SocketAddr, peer: SocketAddr) -> bool {
    let family = word(info, 184) as i32;
    if (family != libc::AF_INET && family != libc::AF_INET6)
        || word(info, 256) != SOCKINFO_TCP
        || word(info, 344) != TCPS_ESTABLISHED
    {
        return false;
    }
    // in_sockinfo's foreign/local ports are network-order values in C ints;
    // its vflag at +24 selects IPv4 (+12 in the address union) or IPv6.
    let foreign_port = u16::from_be(word(info, 264) as u16);
    let local_port = u16::from_be(word(info, 268) as u16);
    let (foreign_ip, local_ip) = if info[288] & 1 != 0 {
        (
            Ipv4Addr::from(<[u8; 4]>::try_from(&info[308..312]).unwrap()).into(),
            Ipv4Addr::from(<[u8; 4]>::try_from(&info[324..328]).unwrap()).into(),
        )
    } else if info[288] & 2 != 0 {
        (
            Ipv6Addr::from(<[u8; 16]>::try_from(&info[296..312]).unwrap()).into(),
            Ipv6Addr::from(<[u8; 16]>::try_from(&info[312..328]).unwrap()).into(),
        )
    } else {
        return false;
    };
    same_endpoint(SocketAddr::new(local_ip, local_port), peer)
        && same_endpoint(SocketAddr::new(foreign_ip, foreign_port), local)
}

fn word(info: &[u8; 1024], offset: usize) -> u32 {
    u32::from_ne_bytes(info[offset..offset + 4].try_into().unwrap())
}

fn executable(pid: i32) -> Option<PathBuf> {
    let mut path = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: path is writable for the declared length, and pid is scalar.
    // The result is copied into an OsString; no buffer pointer escapes.
    let count = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
    if count <= 0 || count as usize > path.len() {
        return None;
    }
    let end = path.iter().position(|byte| *byte == 0)?;
    if end == 0 {
        return None;
    }
    path.truncate(end);
    Some(PathBuf::from(OsString::from_vec(path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libproc_descriptor_layout_is_two_32_bit_fields() {
        assert_eq!(size_of::<ProcFdInfo>(), 8);
        assert_eq!(std::mem::align_of::<SocketInfo>(), 8);
    }
}
