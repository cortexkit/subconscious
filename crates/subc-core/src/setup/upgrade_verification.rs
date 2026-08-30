use std::{fs, path::Path};

use super::model::UpgradeTarget;

/// Facts gathered after activation. The executor prints all of them so an
/// operator can distinguish a downloaded file from the process actually serving
/// the replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationEvidence {
    pub pid: Option<u32>,
    pub inode: String,
    pub healthy: bool,
    pub version: String,
    pub running_image_matches_destination: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationExpectation {
    pub expected_inode: String,
    pub expected_version: String,
    pub require_pid: bool,
    pub require_running_image_match: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VerificationFailure {
    MissingPid,
    InodeMismatch { expected: String, actual: String },
    Unhealthy,
    VersionMismatch { expected: String, actual: String },
    RunningImageMismatch,
}

impl std::fmt::Display for VerificationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingPid => formatter.write_str("no live PID was observed"),
            Self::InodeMismatch { expected, actual } => {
                write!(
                    formatter,
                    "destination inode mismatch: expected {expected}, observed {actual}"
                )
            }
            Self::Unhealthy => formatter.write_str("health check was not healthy"),
            Self::VersionMismatch { expected, actual } => {
                write!(
                    formatter,
                    "version mismatch: expected {expected}, observed {actual}"
                )
            }
            Self::RunningImageMismatch => {
                formatter.write_str("the running image did not match the destination")
            }
        }
    }
}

impl std::error::Error for VerificationFailure {}

pub fn verify_post_activation(
    evidence: &VerificationEvidence,
    expectation: &VerificationExpectation,
) -> Result<String, VerificationFailure> {
    if expectation.require_pid && evidence.pid.is_none() {
        return Err(VerificationFailure::MissingPid);
    }
    if evidence.inode != expectation.expected_inode {
        return Err(VerificationFailure::InodeMismatch {
            expected: expectation.expected_inode.clone(),
            actual: evidence.inode.clone(),
        });
    }
    if !evidence.healthy {
        return Err(VerificationFailure::Unhealthy);
    }
    if evidence.version != expectation.expected_version {
        return Err(VerificationFailure::VersionMismatch {
            expected: expectation.expected_version.clone(),
            actual: evidence.version.clone(),
        });
    }
    if expectation.require_running_image_match && !evidence.running_image_matches_destination {
        return Err(VerificationFailure::RunningImageMismatch);
    }
    Ok(format!(
        "pid={}; inode={}; health=healthy; version={}; running-image=matched",
        evidence
            .pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "not-required".to_string()),
        evidence.inode,
        evidence.version
    ))
}

/// A stable destination identity. Unix reports the real inode; Windows reports
/// the replacement's SHA-256 because the standard library does not expose the
/// platform file index without unsafe FFI. Both values detect a stale destination.
pub fn destination_inode(path: &Path) -> Result<String, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("could not stat {}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(metadata.ino().to_string())
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        super::upgrade_assets::sha256_file(path)
    }
}

pub fn expected_post_activation(
    destination: &Path,
    version: impl Into<String>,
    require_pid: bool,
    require_running_image_match: bool,
) -> Result<VerificationExpectation, String> {
    Ok(VerificationExpectation {
        expected_inode: destination_inode(destination)?,
        expected_version: version.into(),
        require_pid,
        require_running_image_match,
    })
}

pub fn target_verification_label(target: UpgradeTarget) -> &'static str {
    match target {
        UpgradeTarget::SubcMcp | UpgradeTarget::Aft => "module provenance and health",
        UpgradeTarget::Daemon => "service-manager process and daemon health",
        UpgradeTarget::Ck => "replacement binary invocation",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verification_rejects_a_healthy_but_stale_inode() {
        let evidence = VerificationEvidence {
            pid: Some(42),
            inode: "old".to_string(),
            healthy: true,
            version: "1.2.3".to_string(),
            running_image_matches_destination: true,
        };
        let expectation = VerificationExpectation {
            expected_inode: "new".to_string(),
            expected_version: "1.2.3".to_string(),
            require_pid: true,
            require_running_image_match: true,
        };
        assert!(matches!(
            verify_post_activation(&evidence, &expectation),
            Err(VerificationFailure::InodeMismatch { .. })
        ));
    }
}
