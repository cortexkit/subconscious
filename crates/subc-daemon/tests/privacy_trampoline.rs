#[test]
#[cfg_attr(
    any(not(feature = "test-support"), not(target_os = "macos")),
    ignore = "requires macOS and the trampoline fixture target"
)]
fn fixture_resolves_the_same_private_api_as_the_production_trampoline() {
    #[cfg(all(target_os = "macos", feature = "test-support"))]
    {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_privacy-trampoline-fixture"))
            .args(["__disclaim-exec", "--probe"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            subc_os::privacy_identity::TRAMPOLINE_PROBE
        );
    }
}
