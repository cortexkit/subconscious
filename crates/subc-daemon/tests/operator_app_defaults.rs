use subc_daemon::{daemon_config, operator_apps::*};

// Integration tests link the library without cfg(test), so these assertions
// also cover the actual production constants rather than a unit-test override.
#[test]
fn operator_app_production_switches_and_identity_are_fail_closed() {
    assert_eq!(
        [
            MACOS_OPERATOR_APP_ENABLED,
            WINDOWS_OPERATOR_APP_ENABLED,
            LINUX_OPERATOR_APP_ENABLED,
            PHONE_OPERATOR_ENABLED,
        ],
        [false; 4]
    );
    let app = COMPILED_APP;
    assert_eq!(app.id, None);
    assert_eq!(app.macos_bundle_id, "io.cortexkit.alfonso.desktop");
    assert_eq!(
        (app.macos_executable, app.macos_bundle),
        ("Alfonso", "Alfonso.app")
    );
    assert_eq!(app.windows_executable, "Alfonso.exe");
    assert_eq!(app.linux_executable, "alfonso");
    // No platform launches or verifies in production: the publisher, the
    // install locations and the Linux index identity are all unset, so
    // `app.open` answers `app_unknown` on Windows and Linux and the pipe and
    // peer paths stamp nothing.
    assert_eq!(app.windows_publisher, None);
    assert_eq!(app.windows_path, None);
    assert_eq!(app.linux_path, None);
    assert_eq!(app.linux_release_index_identity, None);
    assert!(MACOS_REQUIREMENT.contains("5R5846NBPW"));
    assert!(MACOS_REQUIREMENT.contains("io.cortexkit.alfonso.desktop"));
    for version in [
        None,
        Some("1.2.10"),
        Some("999999.0.0"),
        Some(RELEASE_FLOOR.to_string().as_str()),
    ] {
        assert!(!meets_release_floor(version, &RELEASE_FLOOR));
    }
}

#[test]
fn configuration_cannot_change_operator_app_policy() {
    let plain =
        daemon_config::parse_with_activity(r#"{"version":1,"modules":{}}"#, "subc.jsonc").unwrap();
    let attempted = daemon_config::parse_with_activity(
        r#"{
        "version":1, "modules":{},
        "macos_operator_app_enabled":true, "windows_operator_app_enabled":true,
        "linux_operator_app_enabled":true, "phone_operator_enabled":true,
        "operator_apps":{"macos":true,"windows":true,"linux":true,"phone":true},
        "release_floor":"0.0.0", "macos_requirement":"always"
    }"#,
        "subc.jsonc",
    )
    .unwrap();
    assert_eq!(plain, attempted);
}
