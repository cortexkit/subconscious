# subc-core: env-filtered tracing, production drop counters, connection-file wire_version tripwire

Three production fixes in the subconscious repo (Rust workspace), motivated by a root-cause audit of the wire-v2 flip: route-lifecycle events are invisible in prod, silently dropped frames have no counters, and a wire-version flip fails at TCP instead of at discovery.

## 1. Env-filtered tracing (crates/subc-core/src/main.rs)

`init_tracing()` currently hard-filters at INFO with no env override:
```rust
tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();
```
Replace with an `EnvFilter`-based subscriber: default directive `"info"`, overridable via the standard `RUST_LOG` env var (use `EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))`). Add the `env-filter` feature to the tracing-subscriber dependency if not enabled. Do NOT change any existing log line levels — the point is that an operator can now set `RUST_LOG=subc_core=debug` in the launchd plist to see the existing `debug!` route-open/GOODBYE lifecycle lines without a rebuild.

## 2. Production drop/close counters exposed via server.describe

Today the daemon silently drops several frame classes with at most a debug line, and the only counter is a `#[cfg(test)]` stale-epoch atomic. Add a small shared counter struct (pattern: crates/subc-core/src/observability.rs `ConnectedClients` — plain `Arc<AtomicU64>`s, cloned into the components that need it) with these counters:

- `module_frames_dropped_no_route`: module→client data frame arriving for an absent/reserved/epoch-mismatched module tuple (drop site: crates/subc-core/src/router.rs, the module-side lookup around lines 223-240).
- `client_frames_dropped_stale_route`: client REQUEST dropped because the channel is unbound or bound at a different epoch (router.rs around lines 299-304).
- `client_egress_close_delivery_failed`: bound module RESPONSE could not be enqueued to the client and the client connection was closed (router.rs ~273-292 / forwarding.rs ~1249-1272).
- `goodbye_relay_client_failed`: client-targeted GOODBYE undeliverable → connection-close escalation.
- `goodbye_relay_module_dropped`: module-targeted route-gone GOODBYE dropped on module egress backpressure (control.rs ~453-500).
- `route_released_epoch_fenced` and `route_release_stale_skipped`: epoch-fenced GOODBYE release outcomes.

Wire them at the named sites (verify exact lines at HEAD — cited line numbers are from a recent audit and may have drifted slightly). Increment is the only hot-path cost; do not add locks.

Expose: add an optional field to `ClientControlResponse::ServerDescribe` in crates/subc-control/src/lib.rs:
```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
counters: Option<serde_json::Value>,
```
populated by the daemon with the counter snapshot (stable snake_case keys as above). This is additive and backward/forward compatible (JSON clients ignore unknown fields; absent field deserializes None). Update the control handler that builds ServerDescribe, any golden JSON vectors that pin the server.describe shape (add a second vector WITH counters rather than mutating the existing one if the goldens are drift-tripwires), and the `ck daemon` CLI rendering (crates/subc-core/src/bin/ck.rs) to print the counters table when present.

## 3. Connection-file wire_version tripwire

crates/subc-transport/src/connection_file.rs `ConnectionInfo` (schema stays 1 — do NOT bump schema, the fleet is live):
- Add `#[serde(default, skip_serializing_if = "Option::is_none")] pub wire_version: Option<u8>`.
- Daemon writes `wire_version: Some(subc_protocol::PROTOCOL_VERSION)` when publishing the file (bootstrap write site).
- Reader-side: `validate()` gets a companion check used by CLIENT paths (not the daemon's own write): if `wire_version` is `Some(v)` and `v != subc_protocol::PROTOCOL_VERSION`, fail with a new typed `ConnectionFileError::WireVersionMismatch { file: v, supported: PROTOCOL_VERSION }` whose Display names both versions and says the binary must be upgraded. `None` (older file) stays accepted — the envelope tripwire still covers that case. Watch the dependency direction: if subc-transport cannot depend on subc-protocol's constant cleanly, take the expected version as a parameter with a helper.
- Update every in-repo reader call site that should enforce it: subc-transport's own read helpers, subc-client-rs (consumer + module serve connect paths), and the watchdog's connection-file integrity check in subc-core (it compares the published file; make sure it doesn't false-alarm on the new field).
- Tests: round-trip with and without the field; mismatch fails loud with the typed error; daemon-written file carries wire_version=2. Update any golden connection-file fixtures.

NOTE: the TS and Swift clients get the same reader check in a SEPARATE task — do not touch clients/.

## Verification bar

cargo test -p subc-transport -p subc-core -p subc-control -p subc-client-rs green; cargo clippy --all-targets -D warnings clean natively AND cross-check `cargo clippy -p subc-core -p subc-transport --target x86_64-pc-windows-gnu --all-targets` (workspace rule: Windows-only cfg gaps fail CI); cargo fmt. Run integration tests with a clean env (`env -u SUBC_MODULE_ID -u SUBC_LAUNCH_NONCE`) — leaked module env vars are a known false-failure class. Commit with a clear message per concern (3 commits preferred: tracing, counters, wire_version).
