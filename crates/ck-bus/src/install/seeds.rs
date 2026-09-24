//! The install tooling never writes an nkey seed. Everything it writes is a JWT (public
//! claims plus a signature), a public nkey, or server configuration; the system account
//! identity seed exists only in memory while its public half is read.
//!
//! A seed is recognised by its encoding, `S` then a role letter (`U`, `A`, `O`, `C` or
//! `N`) then at least 54 base32 characters (a whole seed is 58), the same pattern the
//! acceptance harness scans for. The pattern is wider than "decodes as a valid seed" on
//! purpose: a false positive refuses an install, a false negative would leave a private
//! key on disk. JWT claims are base64url, so a seed inside them is invisible in the raw
//! text: every base64url run is also decoded and scanned.

use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

/// Whether `text`, or any base64url run in it once decoded, holds a seed-shaped string.
pub fn holds_seed(text: &str) -> bool {
    if !find_seeds(text).is_empty() {
        return true;
    }
    let base64url = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    text.split(|c: char| !base64url(c))
        .filter(|run| run.len() >= 56)
        .filter_map(|run| URL_SAFE_NO_PAD.decode(run).ok())
        .any(|decoded| !find_seeds(&String::from_utf8_lossy(&decoded)).is_empty())
}

/// Every substring of `text` shaped like an nkey seed.
pub fn find_seeds(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let base32 = |b: u8| b.is_ascii_uppercase() || (b'2'..=b'7').contains(&b);
    let mut found = Vec::new();
    for start in 0..bytes.len().saturating_sub(55) {
        if bytes[start] == b'S'
            && b"UAOCN".contains(&bytes[start + 1])
            && bytes[start + 2..start + 56].iter().all(|b| base32(*b))
        {
            found.push(text[start..start + 56].to_string());
        }
    }
    found
}

/// Refuses when any of `files` contains a seed-shaped string, naming the files (never
/// the seed itself).
pub fn refuse_seeds_in_files(files: &[PathBuf]) -> Result<(), String> {
    let mut offending = Vec::new();
    for path in files {
        let bytes = std::fs::read(path)
            .map_err(|error| format!("read back {}: {error}", path.display()))?;
        if holds_seed(&String::from_utf8_lossy(&bytes)) {
            offending.push(path.display().to_string());
        }
    }
    if offending.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "an nkey seed was found in written file(s): {}",
            offending.join(", ")
        ))
    }
}

/// Refuses when `content`, about to be written to `path`, contains a seed-shaped string.
pub fn refuse_seed_in_content(path: &Path, content: &str) -> Result<(), String> {
    if !holds_seed(content) {
        Ok(())
    } else {
        Err(format!(
            "refusing to write {}: its content contains an nkey seed",
            path.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{find_seeds, refuse_seeds_in_files};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use nkeys::KeyPair;

    #[test]
    fn a_planted_seed_is_found_and_public_keys_are_not() {
        let user = KeyPair::new_user();
        let account = KeyPair::new_account();
        let dir = tempfile::tempdir().unwrap();
        let clean = dir.path().join("clean");
        std::fs::write(
            &clean,
            format!("{}: \"{}\"\n", account.public_key(), user.public_key()),
        )
        .unwrap();
        refuse_seeds_in_files(std::slice::from_ref(&clean)).expect("public keys are not seeds");

        let planted = dir.path().join("planted");
        let seed = user.seed().unwrap();
        std::fs::write(&planted, format!("name: \"{seed}\"\n")).unwrap();
        // Inside a JWT's base64url claims the seed is not visible as text.
        let claims = URL_SAFE_NO_PAD.encode(format!("{{\"name\":\"{seed}\"}}"));
        assert!(find_seeds(&claims).is_empty() && super::holds_seed(&format!("e30.{claims}.sig")));
        let found = find_seeds(&std::fs::read_to_string(&planted).unwrap());
        assert!(found.len() == 1 && seed.starts_with(&found[0]), "{found:?}");
        let error = refuse_seeds_in_files(&[clean, planted]).unwrap_err();
        assert!(
            error.contains("planted") && !error.contains("clean"),
            "{error}"
        );
    }
}
