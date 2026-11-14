//! A deliberately small reader for the public code-requirement blob returned
//! by SecRequirementCopyData. Accept conjunctions of the ordinary identifier
//! and Apple generic anchor with one signer binding: either the leaf team OU
//! (what codesign writes for Developer ID, optionally narrowed by Apple's
//! Developer ID certificate markers), or the leaf common name together with the
//! Apple Worldwide Developer Relations intermediate marker (what codesign
//! writes for an Apple Development certificate). Unknown expressions fail
//! closed.

const MAGIC: u32 = 0xfade_0c00;
const DEV_ID_CA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x63, 0x64, 6, 2, 6];
const DEV_ID_APP: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x63, 0x64, 6, 1, 13];
const WWDR_CA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x63, 0x64, 6, 2, 1];

const IDENTIFIER: u8 = 1;
const ANCHOR: u8 = 2;
const TEAM_OU: u8 = 4;
const LEAF_CN: u8 = 32;
const WWDR_INTERMEDIATE: u8 = 64;

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
    let parsed = reader
        .expression(identifier.as_bytes(), team.as_bytes())
        .is_some()
        && reader.offset == bytes.len();
    // The common name is a signer binding only beside the intermediate marker:
    // together they name one Apple-issued development certificate. Its value
    // needs no check here, because the caller validates the running signature
    // against this same requirement.
    let signer = reader.seen & TEAM_OU != 0
        || reader.seen & (LEAF_CN | WWDR_INTERMEDIATE) == LEAF_CN | WWDR_INTERMEDIATE;
    parsed && reader.seen & (IDENTIFIER | ANCHOR) == IDENTIFIER | ANCHOR && signer
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
            2 if self.data()? == identifier => self.mark(IDENTIFIER),
            15 => self.mark(ANCHOR),
            11 => {
                if self.word()? != 0 {
                    return None;
                }
                let field = self.data()?;
                if self.word()? != 1 {
                    return None;
                }
                let value = self.data()?;
                match field {
                    b"subject.OU" if value == team => self.mark(TEAM_OU),
                    b"subject.CN" if !value.is_empty() => self.mark(LEAF_CN),
                    _ => None,
                }
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
                    (1, WWDR_CA) => self.mark(WWDR_INTERMEDIATE),
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

    fn hex_bytes(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn accepts_apple_development_csreq_compiler_golden() {
        // Compiled by macOS csreq. This is the form codesign writes as the
        // designated requirement for an Apple Development certificate:
        // identifier "io.example.daemon" and anchor apple generic and
        // certificate leaf[subject.CN] = "Apple Development: Example Person
        // (ABCDE12345)" and certificate 1[field.1.2.840.113635.100.6.2.1] exists.
        let bytes = hex_bytes(concat!(
            "fade0c00000000a4000000010000000600000006000000060000000200000011",
            "696f2e6578616d706c652e6461656d6f6e0000000000000f0000000b00000000",
            "0000000a7375626a6563742e434e0000000000010000002e4170706c65204465",
            "76656c6f706d656e743a204578616d706c6520506572736f6e20284142434445",
            "31323334352900000000000e000000010000000a2a864886f763640602010000",
            "00000000"
        ));
        assert!(ordinary(&bytes, "io.example.daemon", "TEAM123456"));
        assert!(!ordinary(&bytes, "other", "TEAM123456"));
        for length in 0..bytes.len() {
            assert!(!ordinary(
                &bytes[..length],
                "io.example.daemon",
                "TEAM123456"
            ));
        }
    }

    /// identifier and anchor, then the given certificate clauses, all joined by AND.
    fn development_blob(common_name: Option<&[u8]>, intermediate: bool) -> Vec<u8> {
        let clauses = 2 + usize::from(common_name.is_some()) + usize::from(intermediate);
        let mut bytes = vec![];
        for value in [MAGIC, 0, 1] {
            word(&mut bytes, value);
        }
        for _ in 1..clauses {
            word(&mut bytes, 6);
        }
        word(&mut bytes, 2);
        data(&mut bytes, b"io.example.daemon");
        word(&mut bytes, 15);
        if let Some(name) = common_name {
            for value in [11, 0] {
                word(&mut bytes, value);
            }
            data(&mut bytes, b"subject.CN");
            word(&mut bytes, 1);
            data(&mut bytes, name);
        }
        if intermediate {
            for value in [14, 1] {
                word(&mut bytes, value);
            }
            data(&mut bytes, WWDR_CA);
            word(&mut bytes, 0);
        }
        let length = bytes.len() as u32;
        bytes[4..8].copy_from_slice(&length.to_be_bytes());
        bytes
    }

    #[test]
    fn a_common_name_binds_the_signer_only_with_the_intermediate_marker() {
        let name: &[u8] = b"Apple Development: Example Person (ABCDE12345)";
        let accept = |blob: Vec<u8>| ordinary(&blob, "io.example.daemon", "TEAM123456");
        assert!(accept(development_blob(Some(name), true)));
        // Either half alone leaves the signer unbound.
        assert!(!accept(development_blob(Some(name), false)));
        assert!(!accept(development_blob(None, true)));
        assert!(!accept(development_blob(Some(b""), true)));
        // Another certificate field is still unknown and fails closed.
        let mut other_field = development_blob(Some(name), true);
        let at = other_field
            .windows(10)
            .position(|w| w == b"subject.CN")
            .unwrap();
        other_field[at + 8..at + 10].copy_from_slice(b"XX");
        assert!(!accept(other_field));
    }
}
