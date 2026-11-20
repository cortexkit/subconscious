//! Which daemon `ck` talks to when no connection file is named.
//!
//! A daemon that crashed can leave its connection file in a directory the
//! next daemon no longer writes to, ahead of the live daemon's file in the
//! discovery order. Discovery passes over such a file when nothing listens
//! at its endpoint and says so. A file whose daemon answers but fails the
//! identity check is never passed over: the error names the other files so
//! the operator can choose one with `--subc`.

use std::path::{Path, PathBuf};

use subc_transport::connection_file;

use super::{CkError, ResolvedConnection};

pub(super) fn discover_connection_file(
    override_path: Option<&Path>,
) -> Result<ResolvedConnection, CkError> {
    let found = connection_file::discover_reachable(override_path).map_err(CkError::from)?;
    for stale in &found.stale {
        eprintln!(
            "warning: skipped stale connection file {}: nothing is listening at its endpoint; using {}",
            stale.display(),
            found.discovered.path.display()
        );
    }
    Ok(ResolvedConnection {
        path: found.discovered.path,
        info: found.discovered.info,
        others: found.others,
    })
}

/// [`authentication_failure`] for the TCP handshake most commands use.
pub(super) fn tcp_authentication_failure(
    error: impl std::fmt::Display,
    others: &[PathBuf],
) -> String {
    authentication_failure("authenticate", error, others)
}

/// The reason given when the chosen daemon fails the identity check, naming
/// any other connection files discovery found but did not try.
pub(super) fn authentication_failure(
    what: &str,
    error: impl std::fmt::Display,
    others: &[PathBuf],
) -> String {
    if others.is_empty() {
        return format!("{what}: {error}");
    }
    let others = others
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{what}: {error}; another connection file exists at {others}, which ck does not try after a failed identity check; pass --subc <file> to choose the daemon"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identity_failure_names_the_files_not_tried() {
        let lone = authentication_failure("authenticate", "bad proof", &[]);
        assert_eq!(lone, "authenticate: bad proof");
        let named = authentication_failure(
            "authenticate",
            "bad proof",
            &[PathBuf::from("/tmp/subc-501.connection.json")],
        );
        assert!(
            named.starts_with("authenticate: bad proof; another connection file exists at /tmp/subc-501.connection.json"),
            "{named}"
        );
        assert!(named.contains("--subc"), "{named}");
    }
}
