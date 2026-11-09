//! A native enumeration binds each parent/name row to its creation identity.

use std::{io, mem::size_of};
use windows_sys::{
    Wdk::System::SystemInformation::{NtQuerySystemInformation, SystemProcessInformation},
    Win32::{
        Foundation::{RtlNtStatusToDosError, STATUS_INFO_LENGTH_MISMATCH},
        System::WindowsProgramming::SYSTEM_PROCESS_INFORMATION,
    },
};

type Entry = (u32, Option<u32>, String, u64);

/// Unlike Toolhelp, this snapshot includes the native creation time. Comparing
/// it with the subsequently opened handle detects PID reuse without consulting
/// the adjustable wall clock.
pub(crate) fn process_entries() -> io::Result<Vec<Entry>> {
    let mut capacity = 64 * 1024_u32;
    for _ in 0..8 {
        // u64 storage provides the native structure's alignment on Windows.
        let mut buffer = vec![0_u64; (capacity as usize).div_ceil(size_of::<u64>())];
        let mut returned = 0;
        // SAFETY: buffer is writable, aligned, and has at least capacity bytes;
        // returned is a live output. No pointers escape this call's allocation.
        let status = unsafe {
            NtQuerySystemInformation(
                SystemProcessInformation,
                buffer.as_mut_ptr().cast(),
                capacity,
                &mut returned,
            )
        };
        if status == STATUS_INFO_LENGTH_MISMATCH {
            capacity = returned
                .max(capacity.checked_mul(2).ok_or_else(invalid_snapshot)?)
                .checked_add(4096)
                .ok_or_else(invalid_snapshot)?;
            continue;
        }
        if status < 0 {
            // SAFETY: this function maps a numeric NTSTATUS to a Win32 error.
            return Err(io::Error::from_raw_os_error(unsafe {
                RtlNtStatusToDosError(status) as i32
            }));
        }
        if returned == 0 || returned > capacity {
            return Err(invalid_snapshot());
        }
        // SAFETY: the initialized allocation covers returned bytes and stays
        // alive while the parser copies out all names and numeric identities.
        let bytes =
            unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), returned as usize) };
        return parse_entries(bytes);
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "process table kept outgrowing its snapshot buffer",
    ))
}

fn invalid_snapshot() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid native process snapshot",
    )
}

fn parse_entries(bytes: &[u8]) -> io::Result<Vec<Entry>> {
    let mut offset = 0_usize;
    let mut entries = Vec::new();
    loop {
        let header_end = offset
            .checked_add(size_of::<SYSTEM_PROCESS_INFORMATION>())
            .filter(|end| *end <= bytes.len())
            .ok_or_else(invalid_snapshot)?;
        // SAFETY: the complete C header is in bounds; unaligned reads also
        // permit byte fixtures. Its fields are integers and raw pointers only.
        let row = unsafe {
            bytes
                .as_ptr()
                .add(offset)
                .cast::<SYSTEM_PROCESS_INFORMATION>()
                .read_unaligned()
        };
        let next = row.NextEntryOffset as usize;
        if next != 0 && (next < header_end - offset || next > bytes.len() - offset) {
            return Err(invalid_snapshot());
        }
        // The SDK reserves these fields, but the SystemProcessInformation ABI
        // places CreateTime at byte 32 and InheritedFromUniqueProcessId directly
        // after UniqueProcessId. CreateTime uses the same FILETIME units as
        // GetProcessTimes. Reserved1 starts at byte 8, hence the 24-byte offset.
        let start = i64::from_ne_bytes(row.Reserved1[24..32].try_into().unwrap());
        let pid = u32::try_from(row.UniqueProcessId as usize).map_err(|_| invalid_snapshot())?;
        let parent = u32::try_from(row.Reserved2 as usize).map_err(|_| invalid_snapshot())?;
        let length = usize::from(row.ImageName.Length);
        if !length.is_multiple_of(2) || row.ImageName.Length > row.ImageName.MaximumLength {
            return Err(invalid_snapshot());
        }
        let name = if length == 0 {
            String::new()
        } else {
            let begin = (row.ImageName.Buffer as usize)
                .checked_sub(bytes.as_ptr() as usize)
                .ok_or_else(invalid_snapshot)?;
            let end = begin
                .checked_add(length)
                .filter(|end| *end <= bytes.len())
                .ok_or_else(invalid_snapshot)?;
            let units: Vec<_> = bytes[begin..end]
                .as_chunks::<2>()
                .0
                .iter()
                .copied()
                .map(u16::from_ne_bytes)
                .collect();
            String::from_utf16_lossy(&units)
        };
        // PID zero is the idle pseudo-process and has no openable identity.
        if pid != 0 {
            entries.push((
                pid,
                Some(parent),
                name,
                u64::try_from(start).map_err(|_| invalid_snapshot())?,
            ));
        }
        if next == 0 {
            return Ok(entries);
        }
        offset += next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<u8> {
        let header = size_of::<SYSTEM_PROCESS_INFORMATION>();
        let mut bytes = vec![0_u8; header * 2 + 4];
        let mut first = SYSTEM_PROCESS_INFORMATION {
            NextEntryOffset: header as u32,
            UniqueProcessId: 42_usize as _,
            Reserved2: 7_usize as _,
            ..Default::default()
        };
        first.Reserved1[24..32].copy_from_slice(&123_456_i64.to_ne_bytes());
        first.ImageName.Length = 4;
        first.ImageName.MaximumLength = 4;
        // SAFETY: the name occupies the final four initialized bytes.
        first.ImageName.Buffer = unsafe { bytes.as_mut_ptr().add(header * 2).cast() };
        let mut second = SYSTEM_PROCESS_INFORMATION {
            UniqueProcessId: 43_usize as _,
            Reserved2: 42_usize as _,
            ..Default::default()
        };
        second.Reserved1[24..32].copy_from_slice(&123_457_i64.to_ne_bytes());
        // SAFETY: both full C headers fit in the initialized byte fixture;
        // write_unaligned does not require the Vec<u8> allocator's alignment.
        unsafe {
            bytes
                .as_mut_ptr()
                .cast::<SYSTEM_PROCESS_INFORMATION>()
                .write_unaligned(first);
            bytes
                .as_mut_ptr()
                .add(header)
                .cast::<SYSTEM_PROCESS_INFORMATION>()
                .write_unaligned(second);
        }
        bytes[header * 2..header * 2 + 2].copy_from_slice(&u16::from(b'a').to_ne_bytes());
        bytes[header * 2 + 2..].copy_from_slice(&u16::from(b'b').to_ne_bytes());
        bytes
    }

    #[test]
    fn linked_native_rows_preserve_creation_parent_and_name() {
        let bytes = fixture();
        assert_eq!(
            parse_entries(&bytes).unwrap(),
            vec![
                (42, Some(7), "ab".into(), 123_456),
                (43, Some(42), String::new(), 123_457),
            ]
        );
    }

    #[test]
    fn malformed_native_rows_never_publish_a_partial_table() {
        let header = size_of::<SYSTEM_PROCESS_INFORMATION>();
        assert!(parse_entries(&[]).is_err());
        let bytes = fixture();
        assert!(parse_entries(&bytes[..header - 1]).is_err());
        assert!(parse_entries(&bytes[..header * 2 - 1]).is_err());
        let mut bad_next = fixture();
        bad_next[..4].copy_from_slice(&1_u32.to_ne_bytes());
        assert!(parse_entries(&bad_next).is_err());
        let mut bad_next = fixture();
        bad_next[..4].copy_from_slice(&u32::MAX.to_ne_bytes());
        assert!(parse_entries(&bad_next).is_err());
        let mut bad_name = fixture();
        let name_offset = std::mem::offset_of!(SYSTEM_PROCESS_INFORMATION, ImageName);
        bad_name[name_offset..name_offset + 2].copy_from_slice(&3_u16.to_ne_bytes());
        assert!(parse_entries(&bad_name).is_err());
        let mut bad_name = fixture();
        bad_name.truncate(header * 2);
        assert!(parse_entries(&bad_name).is_err());
    }

    #[test]
    fn native_enumeration_creation_matches_the_retained_handle() {
        let pid = std::process::id();
        let process = crate::Process::open(pid).unwrap().unwrap();
        let before = process.observe().unwrap().start_time;
        let rows = process_entries().unwrap();
        let row = rows.iter().find(|row| row.0 == pid).unwrap();
        assert_eq!(row.3, before);
        assert_eq!(row.1, crate::parent_pid(pid));
        assert!(!row.2.is_empty());
        assert_eq!(process.observe().unwrap().start_time, before);
    }
}
