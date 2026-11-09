//! Bounded streaming extraction of embedded Mach-O CodeDirectory blobs.
//!
//! Formats are the public mach-o/fat.h, mach-o/loader.h and
//! Security/CSCommon.h layouts. Only CodeDirectory blobs (primary slot zero or
//! alternate slots 0x1000..0x1005) contribute hashes; CMS envelopes do not.

use super::{CodeDirectoryHash, FileSignature, SignatureError};
use std::ops::Range;
#[path = "witness.rs"]
pub(super) mod witness;
const LIMIT: usize = 16 * 1024 * 1024;
const SLICES: usize = 8;
fn bad() -> SignatureError {
    SignatureError::Parse
}
fn word(bytes: &[u8], at: usize, little: bool) -> Result<u32, SignatureError> {
    let value = <[u8; 4]>::try_from(
        bytes
            .get(at..at.checked_add(4).ok_or_else(bad)?)
            .ok_or_else(bad)?,
    )
    .map_err(|_| bad())?;
    Ok(if little {
        u32::from_le_bytes(value)
    } else {
        u32::from_be_bytes(value)
    })
}
fn wide(bytes: &[u8], at: usize) -> Result<u64, SignatureError> {
    Ok(u64::from_be_bytes(
        bytes
            .get(at..at.checked_add(8).ok_or_else(bad)?)
            .ok_or_else(bad)?
            .try_into()
            .map_err(|_| bad())?,
    ))
}
fn bounded(start: u64, length: u64, end: u64) -> Result<Range<u64>, SignatureError> {
    let stop = start.checked_add(length).ok_or_else(bad)?;
    if stop > end {
        return Err(bad());
    }
    Ok(start..stop)
}
#[derive(Default)]
struct Buffer {
    range: Range<u64>,
    bytes: Vec<u8>,
}
impl Buffer {
    fn new(range: Range<u64>) -> Result<Self, SignatureError> {
        if range.end.checked_sub(range.start).ok_or_else(bad)? > LIMIT as u64 {
            return Err(bad());
        }
        Ok(Self {
            range,
            bytes: Vec::new(),
        })
    }
    fn push(&mut self, offset: u64, chunk: &[u8]) -> Result<(), SignatureError> {
        let wanted = self
            .range
            .start
            .checked_add(self.bytes.len() as u64)
            .ok_or_else(bad)?;
        let end = offset.checked_add(chunk.len() as u64).ok_or_else(bad)?;
        if wanted >= self.range.end || wanted >= end {
            return Ok(());
        }
        if wanted < offset {
            return Err(bad());
        }
        let start = usize::try_from(wanted - offset).map_err(|_| bad())?;
        let stop = usize::try_from(self.range.end.min(end) - offset).map_err(|_| bad())?;
        self.bytes
            .extend_from_slice(chunk.get(start..stop).ok_or_else(bad)?);
        Ok(())
    }
    fn complete(&self) -> bool {
        self.bytes.len() as u64 == self.range.end - self.range.start
    }
}
struct Slice {
    range: Range<u64>,
    header: Buffer,
    commands: Option<Buffer>,
    signature: Option<Buffer>,
    command_count: u32,
    little: bool,
    done: bool,
    directories: Vec<Directory>,
}
impl Slice {
    fn new(range: Range<u64>) -> Result<Self, SignatureError> {
        if range.end - range.start < 28 {
            return Err(bad());
        }
        Ok(Self {
            header: Buffer::new(range.start..range.start + 28)?,
            range,
            commands: None,
            signature: None,
            command_count: 0,
            little: false,
            done: false,
            directories: Vec::new(),
        })
    }
    fn push(&mut self, offset: u64, bytes: &[u8]) -> Result<(), SignatureError> {
        if self.done {
            return Ok(());
        }
        self.header.push(offset, bytes)?;
        if self.header.complete() && self.commands.is_none() {
            let magic = self.header.bytes.get(..4).ok_or_else(bad)?;
            let (little, is64) = match magic {
                [0xce, 0xfa, 0xed, 0xfe] => (true, false),
                [0xcf, 0xfa, 0xed, 0xfe] => (true, true),
                [0xfe, 0xed, 0xfa, 0xce] => (false, false),
                [0xfe, 0xed, 0xfa, 0xcf] => (false, true),
                _ => return Err(bad()),
            };
            self.little = little;
            self.command_count = word(&self.header.bytes, 16, little)?;
            let size = word(&self.header.bytes, 20, little)?;
            if self.command_count > 65536
                || self.command_count.checked_mul(8).ok_or_else(bad)? > size
            {
                return Err(bad());
            }
            let start = self
                .range
                .start
                .checked_add(if is64 { 32 } else { 28 })
                .ok_or_else(bad)?;
            self.commands = Some(Buffer::new(bounded(
                start,
                u64::from(size),
                self.range.end,
            )?)?);
        }
        if let Some(commands) = &mut self.commands {
            commands.push(offset, bytes)?;
            if commands.complete() && self.signature.is_none() {
                let mut at = 0usize;
                let mut found = None;
                for _ in 0..self.command_count {
                    let command = word(&commands.bytes, at, self.little)?;
                    let size = usize::try_from(word(
                        &commands.bytes,
                        at.checked_add(4).ok_or_else(bad)?,
                        self.little,
                    )?)
                    .map_err(|_| bad())?;
                    let end = at.checked_add(size).ok_or_else(bad)?;
                    if size < 8 || end > commands.bytes.len() {
                        return Err(bad());
                    }
                    if command == 0x1d {
                        if size != 16 || found.is_some() {
                            return Err(bad());
                        }
                        let relative = u64::from(word(&commands.bytes, at + 8, self.little)?);
                        let length = u64::from(word(&commands.bytes, at + 12, self.little)?);
                        let start = self.range.start.checked_add(relative).ok_or_else(bad)?;
                        if start < commands.range.end || length < 12 {
                            return Err(bad());
                        }
                        found = Some(Buffer::new(bounded(start, length, self.range.end)?)?);
                    }
                    at = end;
                }
                if at != commands.bytes.len() {
                    return Err(bad());
                }
                if found.is_none() {
                    self.done = true;
                    return Ok(());
                }
                self.signature = found;
            }
        }
        if let Some(signature) = &mut self.signature {
            signature.push(offset, bytes)?;
            if signature.complete() {
                self.directories =
                    directories(&signature.bytes, signature.range.start - self.range.start)?;
                self.done = true;
            }
        }
        Ok(())
    }
}

fn directories(bytes: &[u8], code_end: u64) -> Result<Vec<Directory>, SignatureError> {
    let length = usize::try_from(word(bytes, 4, false)?).map_err(|_| bad())?;
    if word(bytes, 0, false)? != 0xfade0cc0 || length < 12 || length > bytes.len() {
        return Err(bad());
    }
    let bytes = &bytes[..length];
    let count = usize::try_from(word(bytes, 8, false)?).map_err(|_| bad())?;
    if count > 64 {
        return Err(bad());
    }
    let index_end = 12usize
        .checked_add(count.checked_mul(8).ok_or_else(bad)?)
        .ok_or_else(bad)?;
    if index_end > bytes.len() {
        return Err(bad());
    }
    let mut ranges = Vec::<Range<usize>>::new();
    let mut blobs = std::collections::BTreeMap::new();
    for index in 0..count {
        let at = 12 + index * 8;
        let slot = word(bytes, at, false)?;
        let start = usize::try_from(word(bytes, at + 4, false)?).map_err(|_| bad())?;
        if start < index_end {
            return Err(bad());
        }
        let length = usize::try_from(word(bytes, start.checked_add(4).ok_or_else(bad)?, false)?)
            .map_err(|_| bad())?;
        let end = start.checked_add(length).ok_or_else(bad)?;
        if length < 8
            || end > bytes.len()
            || ranges
                .iter()
                .any(|range| start < range.end && range.start < end)
        {
            return Err(bad());
        }
        ranges.push(start..end);
        if blobs
            .insert(slot, bytes.get(start..end).ok_or_else(bad)?)
            .is_some()
        {
            return Err(bad());
        }
    }
    let mut directories = Vec::new();
    for (slot, blob) in &blobs {
        if *slot == 0 || (0x1000..0x1005).contains(slot) {
            directories.push(Directory::new(blob, code_end, &blobs)?);
        }
    }
    if directories.is_empty() {
        return Err(bad());
    }
    Ok(directories)
}

struct Directory {
    hash: CodeDirectoryHash,
    kind: u8,
    size: usize,
    limit: u64,
    page: usize,
    slots: Vec<u8>,
    buffer: Vec<u8>,
    consumed: u64,
    slot: usize,
    valid: bool,
}
impl Directory {
    fn new(
        bytes: &[u8],
        code_end: u64,
        blobs: &std::collections::BTreeMap<u32, &[u8]>,
    ) -> Result<Self, SignatureError> {
        if word(bytes, 0, false)? != 0xfade0c02 || bytes.len() < 44 {
            return Err(bad());
        }
        let version = word(bytes, 8, false)?;
        let minimum = match version {
            0x20000..0x20100 => 44,
            0x20100..0x20200 => 48,
            0x20200..0x20300 => 52,
            0x20300..0x20400 => 64,
            0x20400..0x20500 => 88,
            0x20500..0x20600 => 96,
            0x20600 => 108,
            _ => return Err(bad()),
        };
        if bytes.len() < minimum {
            return Err(bad());
        }
        let kind = bytes[37];
        let size = usize::from(bytes[36]);
        if size
            != match kind {
                1 | 3 => 20,
                2 => 32,
                4 => 48,
                5 => 64,
                _ => return Err(bad()),
            }
        {
            return Err(bad());
        }
        let count = usize::try_from(word(bytes, 28, false)?).map_err(|_| bad())?;
        let special = usize::try_from(word(bytes, 24, false)?).map_err(|_| bad())?;
        if count > 131072 || special > 64 {
            return Err(bad());
        }
        let offset = usize::try_from(word(bytes, 16, false)?).map_err(|_| bad())?;
        let first = offset
            .checked_sub(special.checked_mul(size).ok_or_else(bad)?)
            .ok_or_else(bad)?;
        let end = offset
            .checked_add(count.checked_mul(size).ok_or_else(bad)?)
            .ok_or_else(bad)?;
        if first < minimum || end > bytes.len() {
            return Err(bad());
        }
        let identifier = usize::try_from(word(bytes, 20, false)?).map_err(|_| bad())?;
        if identifier < minimum
            || !bytes
                .get(identifier..first)
                .is_some_and(|value| value.contains(&0))
        {
            return Err(bad());
        }
        let short = word(bytes, 32, false)?;
        let limit = if short == u32::MAX && version >= 0x20300 {
            wide(bytes, 56)?
        } else {
            u64::from(short)
        };
        if limit > code_end {
            return Err(bad());
        }
        let shift = bytes[39];
        let page = match shift {
            0 => usize::try_from(limit.max(1)).map_err(|_| bad())?,
            12..=20 => 1usize.checked_shl(u32::from(shift)).ok_or_else(bad)?,
            _ => return Err(bad()),
        };
        if page > 1024 * 1024
            || u64::try_from(count).map_err(|_| bad())? != limit.div_ceil(page as u64)
        {
            return Err(bad());
        }
        let mut valid = !(version >= 0x20100 && word(bytes, 44, false)? != 0);
        for slot in 1..=special {
            let start = offset - slot * size;
            let expected = &bytes[start..start + size];
            if let Some(blob) = blobs.get(&(slot as u32)) {
                valid &= super::hash_bytes(blob, kind)?.get(..size) == Some(expected);
            } else {
                valid &= expected.iter().all(|byte| *byte == 0);
            }
        }
        valid &= !blobs
            .keys()
            .any(|slot| *slot > special as u32 && *slot < 0x1000);
        let hash = super::hash_bytes(bytes, kind)?;
        Ok(Self {
            hash: CodeDirectoryHash(hash[..20].try_into().map_err(|_| bad())?),
            kind,
            size,
            limit,
            page,
            slots: bytes[offset..end].to_vec(),
            buffer: Vec::new(),
            consumed: 0,
            slot: 0,
            valid,
        })
    }
    fn push(&mut self, slice: u64, offset: u64, chunk: &[u8]) -> Result<(), SignatureError> {
        let current = slice.checked_add(self.consumed).ok_or_else(bad)?;
        let end = slice.checked_add(self.limit).ok_or_else(bad)?;
        let chunk_end = offset.checked_add(chunk.len() as u64).ok_or_else(bad)?;
        if current >= end || current >= chunk_end {
            return Ok(());
        }
        if current < offset {
            return Err(bad());
        }
        let start = usize::try_from(current - offset).map_err(|_| bad())?;
        let stop = usize::try_from(end.min(chunk_end) - offset).map_err(|_| bad())?;
        let mut bytes = chunk.get(start..stop).ok_or_else(bad)?;
        if !self.valid {
            self.consumed = self
                .consumed
                .checked_add(bytes.len() as u64)
                .ok_or_else(bad)?;
            return Ok(());
        }
        while !bytes.is_empty() {
            let remaining = self.limit - self.consumed;
            let count = (self.page - self.buffer.len())
                .min(bytes.len())
                .min(usize::try_from(remaining).map_err(|_| bad())?);
            self.buffer.extend_from_slice(&bytes[..count]);
            self.consumed += count as u64;
            bytes = &bytes[count..];
            if self.buffer.len() == self.page || self.consumed == self.limit {
                let start = self.slot.checked_mul(self.size).ok_or_else(bad)?;
                let expected = self
                    .slots
                    .get(start..start.checked_add(self.size).ok_or_else(bad)?)
                    .ok_or_else(bad)?;
                self.valid &=
                    super::hash_bytes(&self.buffer, self.kind)?.get(..self.size) == Some(expected);
                self.buffer.clear();
                self.slot += 1;
            }
        }
        Ok(())
    }
    fn verified(&self) -> bool {
        self.valid
            && self.consumed == self.limit
            && self.buffer.is_empty()
            && self.slot * self.size == self.slots.len()
    }
}

pub(super) struct Capture {
    length: u64,
    prefix: Buffer,
    parsed: bool,
    not_macho: bool,
    slices: Vec<Slice>,
    position: u64,
}
impl Capture {
    pub(super) fn new(length: u64) -> Result<Self, SignatureError> {
        if length < 4 {
            return Ok(Self {
                length,
                prefix: Buffer::new(0..length)?,
                parsed: true,
                not_macho: true,
                slices: Vec::new(),
                position: 0,
            });
        }
        Ok(Self {
            length,
            prefix: Buffer::new(0..4)?,
            parsed: false,
            not_macho: false,
            slices: Vec::new(),
            position: 0,
        })
    }
    pub(super) fn push(&mut self, bytes: &[u8]) -> Result<(), SignatureError> {
        let offset = self.position;
        self.position = self
            .position
            .checked_add(bytes.len() as u64)
            .ok_or_else(bad)?;
        if self.position > self.length {
            return Err(bad());
        }
        if !self.parsed {
            self.prefix.push(offset, bytes)?;
            if self.prefix.complete() {
                let magic = word(&self.prefix.bytes, 0, false)?;
                let fat64 = magic == 0xcafebabf;
                if magic == 0xcafebabe || fat64 {
                    if self.prefix.range.end < 8 {
                        self.prefix.range.end = 8;
                        self.prefix.push(offset, bytes)?;
                    }
                    if self.prefix.complete() {
                        let count = usize::try_from(word(&self.prefix.bytes, 4, false)?)
                            .map_err(|_| bad())?;
                        if count == 0 || count > SLICES {
                            return Err(bad());
                        }
                        let stride = if fat64 { 32 } else { 20 };
                        self.prefix.range.end = u64::try_from(
                            8usize
                                .checked_add(count.checked_mul(stride).ok_or_else(bad)?)
                                .ok_or_else(bad)?,
                        )
                        .map_err(|_| bad())?;
                        if self.prefix.range.end > self.length {
                            return Err(bad());
                        }
                        self.prefix.push(offset, bytes)?;
                        if self.prefix.complete() {
                            let mut ranges = Vec::<Range<u64>>::new();
                            for index in 0..count {
                                let at = 8 + index * stride;
                                let (start, size) = if fat64 {
                                    (
                                        wide(&self.prefix.bytes, at + 8)?,
                                        wide(&self.prefix.bytes, at + 16)?,
                                    )
                                } else {
                                    (
                                        u64::from(word(&self.prefix.bytes, at + 8, false)?),
                                        u64::from(word(&self.prefix.bytes, at + 12, false)?),
                                    )
                                };
                                let range = bounded(start, size, self.length)?;
                                if start < self.prefix.range.end
                                    || ranges
                                        .iter()
                                        .any(|old| start < old.end && old.start < range.end)
                                {
                                    return Err(bad());
                                }
                                self.slices.push(Slice::new(range.clone())?);
                                ranges.push(range);
                            }
                            self.parsed = true;
                        }
                    }
                } else if matches!(magic, 0xcefaedfe | 0xcffaedfe | 0xfeedface | 0xfeedfacf) {
                    let mut slice = Slice::new(0..self.length)?;
                    slice.push(0, &self.prefix.bytes)?;
                    self.slices.push(slice);
                    self.parsed = true;
                } else {
                    self.not_macho = true;
                    self.parsed = true;
                }
            }
        }
        for slice in &mut self.slices {
            slice.push(offset, bytes)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn signed() -> Vec<u8> {
        let mut bytes = vec![0; 168];
        bytes[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
        bytes[16..20].copy_from_slice(&1u32.to_le_bytes());
        bytes[20..24].copy_from_slice(&16u32.to_le_bytes());
        bytes[32..36].copy_from_slice(&0x1du32.to_le_bytes());
        bytes[36..40].copy_from_slice(&16u32.to_le_bytes());
        bytes[40..44].copy_from_slice(&64u32.to_le_bytes());
        bytes[44..48].copy_from_slice(&104u32.to_le_bytes());
        bytes[64..68].copy_from_slice(&0xfade0cc0u32.to_be_bytes());
        bytes[68..72].copy_from_slice(&104u32.to_be_bytes());
        bytes[72..76].copy_from_slice(&1u32.to_be_bytes());
        bytes[80..84].copy_from_slice(&20u32.to_be_bytes());
        bytes[84..88].copy_from_slice(&0xfade0c02u32.to_be_bytes());
        bytes[88..92].copy_from_slice(&84u32.to_be_bytes());
        bytes[92..96].copy_from_slice(&0x20000u32.to_be_bytes());
        bytes[100..104].copy_from_slice(&52u32.to_be_bytes());
        bytes[104..108].copy_from_slice(&44u32.to_be_bytes());
        bytes[112..116].copy_from_slice(&1u32.to_be_bytes());
        bytes[116..120].copy_from_slice(&64u32.to_be_bytes());
        bytes[120] = 32;
        bytes[121] = 2;
        bytes[123] = 12;
        bytes[128..136].copy_from_slice(b"fixture\0");
        let page = super::super::hash_bytes(&bytes[..64], 2).unwrap();
        bytes[136..168].copy_from_slice(&page);
        bytes
    }
    fn parse(bytes: &[u8], chunk: usize) -> Result<FileSignature, SignatureError> {
        let mut reads = vec![0u8; bytes.len()];
        let result = witness::extract(
            bytes.len() as u64,
            |output, offset| {
                let offset = offset as usize;
                let count = output.len().min(bytes.len() - offset);
                output[..count].copy_from_slice(&bytes[offset..offset + count]);
                for read in &mut reads[offset..offset + count] {
                    *read += 1;
                    assert_eq!(*read, 1, "a witness byte was read twice");
                }
                Ok(count)
            },
            |_| {},
            chunk,
        );
        if result.is_ok() {
            assert!(reads.iter().all(|read| *read == 1));
        }
        result
    }
    #[test]
    fn content_binding_streaming_parser_rejects_truncation_counts_and_overlaps_without_panicking() {
        let valid = signed();
        for chunk in 1..=valid.len() {
            assert!(
                matches!(parse(&valid,chunk),Ok(FileSignature::Signed(hashes)) if hashes.len()==1),
                "chunk {chunk}"
            );
        }
        for end in 4..valid.len() {
            assert!(
                matches!(parse(&valid[..end], 7), Err(SignatureError::Parse)),
                "truncated at {end}"
            );
        }
        for (offset, word) in [
            (16, u32::MAX),
            (20, u32::MAX),
            (36, 4),
            (40, u32::MAX),
            (44, u32::MAX),
            (68, u32::MAX),
            (72, u32::MAX),
            (80, 12),
            (88, u32::MAX),
        ] {
            let mut corrupt = valid.clone();
            corrupt[offset..offset + 4].copy_from_slice(&word.to_be_bytes());
            assert!(
                matches!(parse(&corrupt, 3), Err(SignatureError::Parse)),
                "invalid header at {offset}"
            );
        }
        let mut fat = vec![0; 256];
        fat[..4].copy_from_slice(&0xcafebabeu32.to_be_bytes());
        fat[4..8].copy_from_slice(&2u32.to_be_bytes());
        for at in [8, 28] {
            fat[at + 8..at + 12].copy_from_slice(&64u32.to_be_bytes());
            fat[at + 12..at + 16].copy_from_slice(&168u32.to_be_bytes());
        }
        fat[64..232].copy_from_slice(&valid);
        assert!(
            matches!(parse(&fat, 1), Err(SignatureError::Parse)),
            "overlapping slices"
        );
        fat[4..8].copy_from_slice(&9u32.to_be_bytes());
        assert!(
            matches!(parse(&fat, 32), Err(SignatureError::Parse)),
            "slice cap"
        );
        for offset in 0..valid.len() {
            for bit in 0..8 {
                let mut input = valid.clone();
                input[offset] ^= 1 << bit;
                assert!(
                    std::panic::catch_unwind(|| parse(&input, 5)).is_ok(),
                    "panic at {offset}:{bit}"
                );
            }
        }
    }

    #[test]
    fn content_binding_excludes_a_foreign_alternate_directory_and_corrupted_code_pages() {
        let original = signed();
        let directory = original[84..168].to_vec();
        let mut bytes = vec![0u8; 260];
        bytes[..64].copy_from_slice(&original[..64]);
        bytes[44..48].copy_from_slice(&196u32.to_le_bytes());
        bytes[64..68].copy_from_slice(&0xfade0cc0u32.to_be_bytes());
        bytes[68..72].copy_from_slice(&196u32.to_be_bytes());
        bytes[72..76].copy_from_slice(&2u32.to_be_bytes());
        bytes[80..84].copy_from_slice(&28u32.to_be_bytes());
        bytes[84..88].copy_from_slice(&0x1000u32.to_be_bytes());
        bytes[88..92].copy_from_slice(&112u32.to_be_bytes());
        bytes[92..176].copy_from_slice(&directory);
        bytes[176..260].copy_from_slice(&directory);
        let valid_page = super::super::hash_bytes(&bytes[..64], 2).unwrap();
        bytes[144..176].copy_from_slice(&valid_page);
        let mut foreign = bytes[..64].to_vec();
        foreign[50] ^= 1;
        let foreign_page = super::super::hash_bytes(&foreign, 2).unwrap();
        bytes[228..260].copy_from_slice(&foreign_page);
        let valid = CodeDirectoryHash(
            super::super::hash_bytes(&bytes[92..176], 2).unwrap()[..20]
                .try_into()
                .unwrap(),
        );
        let foreign = CodeDirectoryHash(
            super::super::hash_bytes(&bytes[176..260], 2).unwrap()[..20]
                .try_into()
                .unwrap(),
        );
        assert_ne!(valid, foreign);
        assert_eq!(
            parse(&bytes, 7).unwrap(),
            FileSignature::Signed(vec![valid]),
            "an embedded foreign CodeDirectory is not approved executable content"
        );
        let mut corrupt = original;
        corrupt[50] ^= 1;
        assert_eq!(
            parse(&corrupt, 3).unwrap(),
            FileSignature::Invalid,
            "a changed code page must invalidate its CodeDirectory"
        );
    }

    #[test]
    fn content_binding_validates_present_special_blobs_and_requires_absent_slots_to_be_zero() {
        let original = signed();
        let mut bytes = vec![0u8; 252];
        bytes[..64].copy_from_slice(&original[..64]);
        bytes[44..48].copy_from_slice(&188u32.to_le_bytes());
        bytes[64..68].copy_from_slice(&0xfade0cc0u32.to_be_bytes());
        bytes[68..72].copy_from_slice(&188u32.to_be_bytes());
        bytes[72..76].copy_from_slice(&2u32.to_be_bytes());
        bytes[80..84].copy_from_slice(&28u32.to_be_bytes());
        bytes[84..88].copy_from_slice(&2u32.to_be_bytes());
        bytes[88..92].copy_from_slice(&176u32.to_be_bytes());
        bytes[92..144].copy_from_slice(&original[84..136]);
        bytes[96..100].copy_from_slice(&148u32.to_be_bytes());
        bytes[108..112].copy_from_slice(&116u32.to_be_bytes());
        bytes[116..120].copy_from_slice(&2u32.to_be_bytes());
        bytes[240..244].copy_from_slice(&0xfade0c01u32.to_be_bytes());
        bytes[244..248].copy_from_slice(&12u32.to_be_bytes());
        let requirement = super::super::hash_bytes(&bytes[240..252], 2).unwrap();
        bytes[144..176].copy_from_slice(&requirement);
        let page = super::super::hash_bytes(&bytes[..64], 2).unwrap();
        bytes[208..240].copy_from_slice(&page);
        assert!(matches!(parse(&bytes,5),Ok(FileSignature::Signed(hashes)) if hashes.len()==1));
        let mut changed = bytes.clone();
        changed[251] = 1;
        assert_eq!(
            parse(&changed, 11).unwrap(),
            FileSignature::Invalid,
            "changed requirements blob"
        );
        bytes[176] = 1;
        assert_eq!(
            parse(&bytes, 13).unwrap(),
            FileSignature::Invalid,
            "nonzero hash for absent Info.plist slot"
        );
    }
}
