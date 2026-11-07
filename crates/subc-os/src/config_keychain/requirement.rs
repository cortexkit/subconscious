//! A deliberately small reader for the public code-requirement blob returned
//! by SecRequirementCopyData. Accept conjunctions of the ordinary identifier,
//! Apple generic anchor and leaf team OU, optionally narrowed by Apple's
//! Developer ID certificate markers. Unknown expressions fail closed.

const MAGIC: u32 = 0xfade_0c00;
const DEV_ID_CA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x63, 0x64, 6, 2, 6];
const DEV_ID_APP: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x63, 0x64, 6, 1, 13];

pub(super) fn ordinary(bytes: &[u8], identifier: &str, team: &str) -> bool {
    let mut reader = Reader {
        bytes,
        offset: 0,
        clauses: 0,
        seen: 0,
    };
    if reader.word() != Some(MAGIC)
        || reader.word().map(|n| n as usize) != Some(bytes.len())
        || reader.word() != Some(1)
    {
        return false;
    }
    reader
        .expression(identifier.as_bytes(), team.as_bytes())
        .is_some()
        && reader.offset == bytes.len()
        && reader.seen & 7 == 7
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
    clauses: usize,
    seen: u8,
}

impl<'a> Reader<'a> {
    fn word(&mut self) -> Option<u32> {
        let end = self.offset.checked_add(4)?;
        let value = u32::from_be_bytes(self.bytes.get(self.offset..end)?.try_into().ok()?);
        self.offset = end;
        Some(value)
    }

    fn data(&mut self) -> Option<&'a [u8]> {
        let length = self.word()? as usize;
        let end = self.offset.checked_add(length)?;
        let padded = end.checked_add(3)? & !3;
        let value = self.bytes.get(self.offset..end)?;
        if self.bytes.get(end..padded)?.iter().any(|byte| *byte != 0) {
            return None;
        }
        self.offset = padded;
        Some(value)
    }

    fn mark(&mut self, bit: u8) -> Option<()> {
        if self.seen & bit != 0 {
            return None;
        }
        self.seen |= bit;
        Some(())
    }

    fn expression(&mut self, identifier: &[u8], team: &[u8]) -> Option<()> {
        // Bound both recursion depth and work, independently of blob size.
        self.clauses += 1;
        if self.clauses > 9 {
            return None;
        }
        match self.word()? {
            6 => {
                self.expression(identifier, team)?;
                self.expression(identifier, team)
            }
            2 if self.data()? == identifier => self.mark(1),
            15 => self.mark(2),
            11 if self.word()? == 0
                && self.data()? == b"subject.OU"
                && self.word()? == 1
                && self.data()? == team =>
            {
                self.mark(4)
            }
            14 => {
                let slot = self.word()?;
                let oid = self.data()?;
                if self.word()? != 0 {
                    return None;
                }
                match (slot, oid) {
                    (1, DEV_ID_CA) => self.mark(8),
                    (0, DEV_ID_APP) => self.mark(16),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_be_bytes());
    }
    fn data(out: &mut Vec<u8>, value: &[u8]) {
        word(out, value.len() as u32);
        out.extend_from_slice(value);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    fn blob(extra: bool) -> Vec<u8> {
        let mut bytes = vec![];
        for value in [MAGIC, 0, 1, 6, 6, 2] {
            word(&mut bytes, value);
        }
        data(&mut bytes, b"io.example.daemon");
        word(&mut bytes, 15);
        if extra {
            word(&mut bytes, 6);
        }
        for value in [11, 0] {
            word(&mut bytes, value);
        }
        data(&mut bytes, b"subject.OU");
        word(&mut bytes, 1);
        data(&mut bytes, b"TEAM123456");
        if extra {
            for value in [6, 14, 1] {
                word(&mut bytes, value);
            }
            data(&mut bytes, DEV_ID_CA);
            for value in [0, 14, 0] {
                word(&mut bytes, value);
            }
            data(&mut bytes, DEV_ID_APP);
            word(&mut bytes, 0);
        }
        let length = bytes.len() as u32;
        bytes[4..8].copy_from_slice(&length.to_be_bytes());
        bytes
    }

    #[test]
    fn accepts_only_structural_team_identifier_and_apple_anchor_conjunctions() {
        for extra in [false, true] {
            let bytes = blob(extra);
            assert!(ordinary(&bytes, "io.example.daemon", "TEAM123456"));
            assert!(!ordinary(&bytes, "other", "TEAM123456"));
            assert!(!ordinary(&bytes, "io.example.daemon", "OTHERTEAM0"));
            for length in 0..bytes.len() {
                assert!(!ordinary(
                    &bytes[..length],
                    "io.example.daemon",
                    "TEAM123456"
                ));
            }
        }
    }

    #[test]
    fn broader_unknown_or_malformed_requirements_fail_closed() {
        let bytes = blob(false);
        // Replacing conjunction with OR/true/unknown/negation must not weaken it.
        for opcode in [0u32, 1, 7, 9, 0x4000_0006, 0x8000_0006, 255] {
            let mut invalid = bytes.clone();
            invalid[12..16].copy_from_slice(&opcode.to_be_bytes());
            assert!(!ordinary(&invalid, "io.example.daemon", "TEAM123456"));
        }
        let mut trailing = bytes.clone();
        trailing.extend_from_slice(&[0; 4]);
        assert!(!ordinary(&trailing, "io.example.daemon", "TEAM123456"));
        let mut kind = bytes.clone();
        kind[11] = 2;
        assert!(!ordinary(&kind, "io.example.daemon", "TEAM123456"));
        let mut missing = vec![];
        for value in [MAGIC, 16, 1, 1] {
            word(&mut missing, value);
        }
        assert!(!ordinary(&missing, "io.example.daemon", "TEAM123456"));
    }

    #[test]
    fn accepts_public_csreq_compiler_golden() {
        // Compiled by macOS csreq, not by this module's test blob builder:
        // identifier "io.example.daemon" and anchor apple generic and
        // certificate leaf[subject.OU] = "TEAM123456".
        let hex = concat!(
            "fade0c00000000600000000100000006000000060000000200000011",
            "696f2e6578616d706c652e6461656d6f6e0000000000000f0000000b",
            "000000000000000a7375626a6563742e4f550000000000010000000a",
            "5445414d3132333435360000"
        );
        let bytes: Vec<_> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        assert!(ordinary(&bytes, "io.example.daemon", "TEAM123456"));
    }
}
