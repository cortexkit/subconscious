use subc_client_rs::consumer::{is_daemon_refusal_before_forward, trusts_daemon_origin};
use subc_protocol::{Flags, Priority};

#[test]
fn raw_error_proof_requires_trusted_version_label_and_pre_forward_code() {
    let unmarked = Flags::new(false, Priority::Interactive, false);
    for version in ["0.37.14", "0.37.15", "0.38.0", "0.38.0-rc1", "unknown"] {
        for flags in [unmarked, unmarked.with_daemon_origin()] {
            for code in [
                "module_reloading",
                "unknown_channel",
                "stale_route_epoch",
                "target_unavailable",
                "module_warming",
                "handler_failed",
            ] {
                let expected = matches!(version, "0.37.15" | "0.38.0")
                    && flags.is_daemon_origin()
                    && matches!(
                        code,
                        "module_reloading" | "unknown_channel" | "stale_route_epoch"
                    );
                assert_eq!(
                    is_daemon_refusal_before_forward(trusts_daemon_origin(version), flags, code),
                    expected,
                    "version={version}, flags={flags:?}, code={code}"
                );
            }
        }
    }
}
