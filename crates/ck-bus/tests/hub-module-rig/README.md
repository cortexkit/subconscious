# Shared hub streams rig

The standalone tests use test-defined grants for access to shared hub streams, without calling ck-bus's credential issuer. They use subject names from `cortexkit-bus-naming` 0.5.8. From the repository root, with stock `nats-server` on `PATH` (or set `NATS_SERVER_BIN` to its executable), run `cargo test --locked --manifest-path crates/ck-bus/tests/hub-module-rig/Cargo.toml --test hub_module_imports rig9_ -- --ignored --nocapture`.

The production issuance arm shares the same server setup and denial probes, but connects with an actual ck-bus-issued JWT: `cargo test --locked -p ck-bus --test hub_module_issuance hub_credential_ -- --include-ignored`. These tests use two temporary loopback servers; they do not need a running installation.
