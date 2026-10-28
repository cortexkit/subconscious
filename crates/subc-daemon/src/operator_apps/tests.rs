use super::*;

#[test]
fn release_floor_uses_numeric_semver_and_rejects_unknown_versions() {
    let floor = Version::new(1, 2, 10);
    for version in ["1.2.10", "1.2.11", "1.3.0", "1.2.10+build.1"] {
        assert!(meets_release_floor(Some(version), &floor), "{version}");
    }
    for version in [
        None,
        Some("1.2.9"),
        Some("1.2.10-rc.1"),
        Some("1.2"),
        Some("oops"),
    ] {
        assert!(!meets_release_floor(version, &floor), "{version:?}");
    }
}

#[test]
fn advisory_peer_facts_cannot_produce_a_stamp() {
    let policy: OperatorAppPolicy = TestOperatorAppPolicy {
        macos_enabled: true,
        windows_enabled: false,
        linux_enabled: false,
        app: "test-app",
        requirement: "always",
        floor: Version::new(1, 2, 10),
        windows_publisher: None,
        windows_path: None,
        linux_path: None,
        windows_breakaway: true,
        linux_force_direct: false,
    }
    .into();
    let peer = ObservedPeer::Observed(subc_os::local_peer::PeerObservation {
        user: subc_os::local_peer::PeerUser::Uid(501),
        pid: 42,
        exe: Some("/Applications/Alfonso.app/Contents/MacOS/Alfonso".into()),
        signing_id: Some("io.cortexkit.alfonso.desktop".into()),
    });
    assert!(policy.stamp(peer.into()).is_none());
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use subc_os::local_peer::fixture::SignedPeerFixture;

    pub(super) const REQUIREMENT: &str = concat!(
        "identifier \"io.cortexkit.alfonso.fixture\" ",
        "and entitlement[\"com.apple.security.get-task-allow\"] absent ",
        "and entitlement[\"com.apple.security.cs.disable-library-validation\"] absent ",
        "and entitlement[\"com.apple.security.cs.allow-dyld-environment-variables\"] absent"
    );

    fn policy() -> OperatorAppPolicy {
        TestOperatorAppPolicy {
            macos_enabled: true,
            windows_enabled: false,
            linux_enabled: false,
            app: "test-app",
            requirement: REQUIREMENT,
            floor: Version::new(1, 2, 10),
            windows_publisher: None,
            windows_path: None,
            linux_path: None,
            windows_breakaway: true,
            linux_force_direct: false,
        }
        .into()
    }

    #[test]
    fn macos_native_requirement_floor_and_restart() {
        let root = cortexkit_test_support::ScratchDir::new("operator-app");
        let mut fixture = SignedPeerFixture::new(
            &root.join("good"),
            "io.cortexkit.alfonso.fixture",
            Some("1.2.10"),
            true,
            None,
        );
        let (stream, peer) = fixture.connect();
        assert!(
            matches!(
                peer.check_code_requirement(REQUIREMENT),
                CodeRequirementOutcome::Satisfied(_)
            ),
            "{:?}",
            peer.check_code_requirement(REQUIREMENT)
        );
        let stamp = policy()
            .stamp(peer.clone())
            .expect("test-signed fixture at floor");
        assert_eq!(stamp.app.pid, fixture.pid());
        assert!(stamp.revalidate());
        std::fs::write(root.join("good").join("Info.plist"), "<plist><dict><key>CFBundleShortVersionString</key><string>1.2.9</string></dict></plist>").unwrap();
        assert!(
            stamp.revalidate(),
            "an exterior plist cannot replace the signed version source"
        );
        assert!(OperatorAppPolicy::default().stamp(peer.clone()).is_none());
        // The production Apple-chain and Team ID clauses must reject an ad-hoc fixture.
        for requirement in [
            MACOS_REQUIREMENT,
            "anchor apple generic",
            "certificate 1[field.1.2.840.113635.100.6.2.6] exists",
            "certificate leaf[field.1.2.840.113635.100.6.1.13] exists",
            "certificate leaf[subject.OU] = \"5R5846NBPW\"",
            "identifier \"wrong.identifier\"",
        ] {
            assert_eq!(
                peer.check_code_requirement(requirement),
                CodeRequirementOutcome::NotSatisfied,
                "{requirement}"
            );
            let mut mutated = policy();
            mutated.requirement = requirement;
            assert!(mutated.stamp(peer.clone()).is_none(), "{requirement}");
        }
        drop(stamp);
        drop(stream);
        // A new daemon has no launch history: reconnect the same live process
        // and derive its eligibility again from the newly captured token.
        let (_stream, peer) = fixture.connect();
        let stamp = policy().stamp(peer).expect("same app after restart");
        assert_eq!(stamp.app.pid, fixture.pid());
        for (label, identifier, version, runtime, entitlement) in [
            (
                "lower",
                "io.cortexkit.alfonso.fixture",
                Some("1.2.9"),
                true,
                None,
            ),
            ("missing", "io.cortexkit.alfonso.fixture", None, true, None),
            (
                "malformed",
                "io.cortexkit.alfonso.fixture",
                Some("banana"),
                true,
                None,
            ),
            ("dev", "dev.cortexkit.alfonso", Some("1.2.10"), true, None),
            (
                "runtime",
                "io.cortexkit.alfonso.fixture",
                Some("1.2.10"),
                false,
                None,
            ),
            (
                "debug",
                "io.cortexkit.alfonso.fixture",
                Some("1.2.10"),
                true,
                Some(("com.apple.security.get-task-allow", true)),
            ),
            (
                "library",
                "io.cortexkit.alfonso.fixture",
                Some("1.2.10"),
                true,
                Some(("com.apple.security.cs.disable-library-validation", true)),
            ),
            (
                "dyld",
                "io.cortexkit.alfonso.fixture",
                Some("1.2.10"),
                true,
                Some((
                    "com.apple.security.cs.allow-dyld-environment-variables",
                    true,
                )),
            ),
        ] {
            let mut fixture = SignedPeerFixture::new(
                &root.join(label),
                identifier,
                version,
                runtime,
                entitlement,
            );
            let (_stream, peer) = fixture.connect();
            assert!(policy().stamp(peer).is_none(), "{label}");
        }
        // Presence is disallowed even when an entitlement's value is false.
        for (label, key) in [
            ("debug-false", "com.apple.security.get-task-allow"),
            (
                "library-false",
                "com.apple.security.cs.disable-library-validation",
            ),
            (
                "dyld-false",
                "com.apple.security.cs.allow-dyld-environment-variables",
            ),
        ] {
            let mut fixture = SignedPeerFixture::new(
                &root.join(label),
                "io.cortexkit.alfonso.fixture",
                Some("1.2.10"),
                true,
                Some((key, false)),
            );
            let (_stream, peer) = fixture.connect();
            assert!(policy().stamp(peer).is_none(), "{label}");
        }
        drop(fixture);
        assert!(
            !stamp.revalidate(),
            "an exited incarnation cannot revalidate"
        );
    }
}
