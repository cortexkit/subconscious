use std::{fs, path::Path};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

use super::{PayloadKind, TRUST_FILES};

pub(super) fn scan_written_trust(nats_dir: &Path) -> Result<(), String> {
    let paths = TRUST_FILES
        .map(|name| nats_dir.join(name))
        .into_iter()
        .chain([
            PayloadKind::Operator.file(nats_dir),
            PayloadKind::SystemAccount.file(nats_dir),
        ]);
    for path in paths {
        let bytes = fs::read(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        if holds_seed(&bytes) {
            return Err(format!(
                "an nkey seed was found in written file: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn holds_seed(bytes: &[u8]) -> bool {
    const SEED_PREFIX_LEN: usize = 56;

    fn seed_shaped(bytes: &[u8]) -> bool {
        bytes.windows(SEED_PREFIX_LEN).any(|run| {
            run[0] == b'S'
                && b"UAOCN".contains(&run[1])
                && run[2..]
                    .iter()
                    .all(|b| b.is_ascii_uppercase() || (b'2'..=b'7').contains(b))
        })
    }

    // Like ck-bus, check the shape without requiring a valid checksum. A seed can
    // be visible in the file or hidden in a base64url JWT claim.
    seed_shaped(bytes)
        || bytes
            .split(|&b| !(b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
            .filter(|run| run.len() >= SEED_PREFIX_LEN)
            .any(|run| {
                URL_SAFE_NO_PAD
                    .decode(run)
                    .is_ok_and(|decoded| seed_shaped(&decoded))
            })
}

/// Named input and expected-result triples shared by setup and ck-bus detector tests.
#[cfg(any(test, feature = "test-support"))]
pub fn seed_detector_test_vectors() -> [(&'static str, String, bool); 3] {
    let raw = format!("SU{}", "ABCDEFG2".repeat(7));
    let short = raw[..55].to_owned();
    let claim = URL_SAFE_NO_PAD.encode(format!(r#"{{"name":"{raw}"}}"#));
    [
        ("raw seed shape", raw, true),
        ("55-character near miss", short, false),
        ("base64url-encoded claim", format!("e30.{claim}.sig"), true),
    ]
}

#[cfg(test)]
mod tests {
    use super::{holds_seed, seed_detector_test_vectors};

    #[test]
    fn seed_detector_matches_shared_vectors() {
        for (name, text, expected) in seed_detector_test_vectors() {
            assert_eq!(holds_seed(text.as_bytes()), expected, "{name}");
        }
    }
}
