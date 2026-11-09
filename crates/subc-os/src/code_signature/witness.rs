//! Read metadata once, then replay those owned bytes into the ordered witness.
//! This makes tail-resident slot tables available before code pages arrive,
//! without rereading any byte from a potentially mutable file.

use super::*;
use std::io;
const METADATA_LIMIT: usize = 16 * 1024 * 1024;
struct Cached {
    start: u64,
    bytes: Vec<u8>,
}
struct Cache<F> {
    read: F,
    ranges: Vec<Cached>,
    metadata: usize,
}
impl<F: FnMut(&mut [u8], u64) -> io::Result<usize>> Cache<F> {
    fn fill(
        &mut self,
        offset: u64,
        output: &mut [u8],
        metadata: bool,
    ) -> Result<(), SignatureError> {
        let mut used = 0usize;
        while used < output.len() {
            let position = offset.checked_add(used as u64).ok_or_else(bad)?;
            if let Some(range) = self.ranges.iter().find(|range| {
                position >= range.start && position - range.start < range.bytes.len() as u64
            }) {
                let start = usize::try_from(position - range.start).map_err(|_| bad())?;
                let count = (range.bytes.len() - start).min(output.len() - used);
                output[used..used + count].copy_from_slice(&range.bytes[start..start + count]);
                used += count;
            } else {
                let available = self
                    .ranges
                    .iter()
                    .filter_map(|range| {
                        range
                            .start
                            .checked_sub(position)
                            .filter(|distance| *distance > 0)
                    })
                    .min()
                    .unwrap_or((output.len() - used) as u64);
                let count = usize::try_from(available.min((output.len() - used) as u64))
                    .map_err(|_| bad())?;
                let count = if metadata {
                    count.min(METADATA_LIMIT.checked_sub(self.metadata).ok_or_else(bad)?)
                } else {
                    count
                };
                if count == 0 {
                    return Err(bad());
                }
                let count = (self.read)(&mut output[used..used + count], position)
                    .map_err(SignatureError::Other)?;
                if count == 0 || count > output.len() - used {
                    return Err(SignatureError::Other(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "code witness changed during capture",
                    )));
                }
                if metadata {
                    self.metadata = self.metadata.checked_add(count).ok_or_else(bad)?;
                    if self.metadata > METADATA_LIMIT {
                        return Err(bad());
                    }
                }
                used += count;
            }
        }
        Ok(())
    }
    fn metadata(&mut self, range: Range<u64>) -> Result<Vec<u8>, SignatureError> {
        let length = usize::try_from(range.end.checked_sub(range.start).ok_or_else(bad)?)
            .map_err(|_| bad())?;
        if length > METADATA_LIMIT {
            return Err(bad());
        }
        let mut bytes = vec![0; length];
        self.fill(range.start, &mut bytes, true)?;
        self.ranges.push(Cached {
            start: range.start,
            bytes: bytes.clone(),
        });
        Ok(bytes)
    }
}

pub(crate) fn extract(
    length: u64,
    read: impl FnMut(&mut [u8], u64) -> io::Result<usize>,
    mut feed: impl FnMut(&[u8]),
    chunk: usize,
) -> Result<FileSignature, SignatureError> {
    if length > 512 * 1024 * 1024 || chunk == 0 || chunk > 1024 * 1024 {
        return Err(bad());
    }
    let mut cache = Cache {
        read,
        ranges: Vec::new(),
        metadata: 0,
    };
    let mut capture = Capture::new(length)?;
    let prefix = cache.metadata(0..length.min(512))?;
    capture.push(&prefix)?;
    if !capture.parsed {
        return Err(bad());
    }
    for slice in &mut capture.slices {
        while !slice.done {
            let range = if !slice.header.complete() {
                slice.header.range.clone()
            } else if let Some(commands) = slice
                .commands
                .as_ref()
                .filter(|commands| !commands.complete())
            {
                commands.range.clone()
            } else if let Some(signature) = slice
                .signature
                .as_ref()
                .filter(|signature| !signature.complete())
            {
                signature.range.clone()
            } else {
                return Err(bad());
            };
            let bytes = cache.metadata(range.clone())?;
            slice.push(range.start, &bytes)?;
        }
        slice.header.bytes.clear();
        if let Some(commands) = &mut slice.commands {
            commands.bytes.clear();
        }
        if let Some(signature) = &mut slice.signature {
            signature.bytes.clear();
        }
    }
    let mut position = 0u64;
    let mut buffer = vec![0; chunk];
    while position < length {
        let count = usize::try_from((length - position).min(chunk as u64)).map_err(|_| bad())?;
        let bytes = &mut buffer[..count];
        cache.fill(position, bytes, false)?;
        feed(bytes);
        for slice in &mut capture.slices {
            for directory in &mut slice.directories {
                directory.push(slice.range.start, position, bytes)?;
            }
        }
        position = position.checked_add(count as u64).ok_or_else(bad)?;
    }
    if capture.not_macho {
        return Ok(FileSignature::NotMachO);
    }
    let mut hashes = Vec::new();
    let mut unsigned_slice = false;
    for slice in capture.slices {
        unsigned_slice |= slice.signature.is_none();
        let valid: Vec<_> = slice
            .directories
            .into_iter()
            .filter(Directory::verified)
            .map(|directory| directory.hash)
            .collect();
        if slice.signature.is_some() && valid.is_empty() {
            return Ok(FileSignature::Invalid);
        }
        for hash in valid {
            if !hashes.contains(&hash) {
                hashes.push(hash);
            }
        }
    }
    Ok(if hashes.is_empty() {
        FileSignature::Unsigned
    } else if unsigned_slice {
        FileSignature::Invalid
    } else {
        FileSignature::Signed(hashes)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn content_binding_metadata_budget_is_enforced_before_reading_past_it() {
        let reads = std::cell::Cell::new(0usize);
        let mut cache = Cache {
            read: |output: &mut [u8], _offset| {
                reads.set(reads.get() + output.len());
                output.fill(0);
                Ok(output.len())
            },
            ranges: Vec::new(),
            metadata: 0,
        };
        cache.metadata(0..METADATA_LIMIT as u64).unwrap();
        assert!(matches!(
            cache.metadata(METADATA_LIMIT as u64..METADATA_LIMIT as u64 + 1),
            Err(SignatureError::Parse)
        ));
        assert_eq!(
            reads.get(),
            METADATA_LIMIT,
            "rejected metadata must not read beyond the aggregate budget"
        );
    }
}
