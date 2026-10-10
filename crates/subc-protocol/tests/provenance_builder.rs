//! `ManifestProvenance` is `#[non_exhaustive]`, so code outside this crate can
//! only build it through its public constructor and setters. These tests live
//! outside the crate on purpose: they prove every field a module could set
//! with a struct literal is still settable, and they would stop compiling if a
//! setter went missing.

use serde_json::json;
use subc_protocol::manifest::{BuildGitShaAbsenceReason, LaunchNonceSource, ManifestProvenance};

#[test]
fn every_provenance_field_is_settable_through_the_public_api() {
    let provenance = ManifestProvenance::new()
        .with_build_git_sha(Some("0123456789abcdef0123456789abcdef01234567".to_string()))
        .with_build_lock_digest(Some("ab".repeat(32)))
        .with_wire_crate_version(Some("0.26.0".to_string()))
        .with_store_schema_version(Some("7".to_string()))
        .with_launch_nonce_source(Some(LaunchNonceSource::Fd));
    assert_eq!(
        serde_json::to_value(&provenance).unwrap(),
        json!({
            "build_git_sha": "0123456789abcdef0123456789abcdef01234567",
            "build_lock_digest": "ab".repeat(32),
            "wire_crate_version": "0.26.0",
            "store_schema_version": "7",
            "launch_nonce_source": "fd",
        })
    );
    assert_eq!(
        provenance.build_git_sha.as_deref(),
        Some("0123456789abcdef0123456789abcdef01234567")
    );
    assert_eq!(provenance.launch_nonce_source, Some(LaunchNonceSource::Fd));

    // The absence reason is only valid without a commit, so it gets its own block.
    let absent = ManifestProvenance::new()
        .with_build_git_sha_absence_reason(Some(BuildGitShaAbsenceReason::NoGitDir));
    assert_eq!(
        serde_json::to_value(&absent).unwrap(),
        json!({ "build_git_sha_absence_reason": "no_git_dir" })
    );
    assert!(absent.validate().is_ok());
}

#[test]
fn launch_nonce_source_round_trips_fd_and_env() {
    for (source, wire) in [
        (LaunchNonceSource::Fd, "fd"),
        (LaunchNonceSource::Env, "env"),
    ] {
        let provenance = ManifestProvenance::new().with_launch_nonce_source(Some(source.clone()));
        let encoded = serde_json::to_value(&provenance).unwrap();
        assert_eq!(encoded, json!({ "launch_nonce_source": wire }));
        let decoded: ManifestProvenance = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.launch_nonce_source, Some(source));
    }
}

#[test]
fn launch_nonce_source_pipe_is_known_and_round_trips() {
    let provenance =
        ManifestProvenance::new().with_launch_nonce_source(Some(LaunchNonceSource::Pipe));
    let encoded = serde_json::to_value(&provenance).unwrap();
    assert_eq!(encoded, json!({ "launch_nonce_source": "pipe" }));
    let decoded: ManifestProvenance = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded.launch_nonce_source, Some(LaunchNonceSource::Pipe));
    assert_eq!(
        LaunchNonceSource::from_wire_name("pipe"),
        LaunchNonceSource::Pipe
    );
}

#[test]
fn an_unset_launch_nonce_source_is_omitted_and_an_absent_one_decodes_as_none() {
    let encoded = serde_json::to_value(ManifestProvenance::new()).unwrap();
    assert_eq!(encoded, json!({}));
    let decoded: ManifestProvenance =
        serde_json::from_value(json!({ "wire_crate_version": "0.25.2" })).unwrap();
    assert_eq!(decoded.launch_nonce_source, None);
}

/// A decoder that predates a provenance field drops it rather than refusing
/// the block: that is how an older daemon reads a module reporting
/// `launch_nonce_source`. The same leniency is what this decoder shows a
/// field newer than itself.
#[test]
fn provenance_decoding_ignores_fields_it_does_not_know() {
    let decoded: ManifestProvenance = serde_json::from_value(json!({
        "wire_crate_version": "0.26.0",
        "launch_nonce_source": "fd",
        "some_future_fact": "x",
    }))
    .expect("an unknown provenance member must not reject the block");
    assert_eq!(decoded.wire_crate_version.as_deref(), Some("0.26.0"));
    assert_eq!(decoded.launch_nonce_source, Some(LaunchNonceSource::Fd));
}

#[test]
fn an_unknown_launch_nonce_source_is_kept_not_refused() {
    let decoded: ManifestProvenance =
        serde_json::from_value(json!({ "launch_nonce_source": "keychain" })).unwrap();
    assert_eq!(
        decoded.launch_nonce_source,
        Some(LaunchNonceSource::ForwardCompatibleUnknown(
            "keychain".to_string()
        ))
    );
    assert_eq!(
        serde_json::to_value(&decoded).unwrap(),
        json!({ "launch_nonce_source": "keychain" })
    );
}
