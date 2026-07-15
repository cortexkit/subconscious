# Task: add catalog.list to SubcConsumer (subc-client-rs)

Repo: ~/Work/Projects/CortexKit/subconscious, off master HEAD (≥ b61f6c1d). Scope: crates/subc-client-rs only. Do NOT spawn or use any subagents.

GOAL: a first-class channel-0 catalog surface on the managed Rust consumer, so TUI/app consumers can fetch module manifests (including ToolProvider tool definitions with schemas) without hand-rolling the transport. Parity precedent: the TS client's catalogList (clients/subc-client/src/client.ts) and the Swift client's catalog fetch — mirror their semantics, not their shape.

API (public, on SubcConsumer):
```rust
/// Fetch the daemon's module catalog over channel 0.
pub async fn catalog_list(&self) -> Result<CatalogList, CallError>;
```
- `CatalogList` = typed mirror of the control-plane response (subc-control's catalog.list reply): the modules vec with module_id, version, provides (ProviderRole incl. ToolProvider tools with name/description/schema/execution_mode), plus whatever the wire carries (connection/liveness fields) — reuse the existing subc-control / subc-protocol types where they're already exported rather than redefining; if the exact response type isn't importable, define a minimal typed struct in subc-client-rs that deserializes the same JSON (serde, tolerant of unknown fields with #[serde(default)] where optional).
- NO module-id filter param on the wire (the op returns all); do not add a filter argument — callers filter. Keep the API surface minimal.

IMPLEMENTATION CONSTRAINTS:
- Ride the EXISTING channel-0 control machinery the consumer already uses for route.open (crates/subc-client-rs/src/consumer.rs — the control request/correlation path). Do not open sockets, do not duplicate handshake/framing. If the internal control-call helper is route.open-specific, generalize it minimally (a private fn control_call(body) -> reply) rather than copy-pasting.
- Deadline: honor the consumer's configured call deadline exactly like other pre-send-bounded ops (deadline-bounded wait on reconnect/writer capacity; NotSent on expiry) — reuse the existing deadline plumbing.
- Retry/classification: catalog.list is read-only + idempotent. On a TRANSIENT failure (reconnect in progress / connection dropped mid-call) it may retry in place within the deadline, consistent with how the consumer treats retryable route.open codes; a definitive daemon error surfaces as CallError::Module.
- Works on a consumer that has NO routes open (channel-0 only) — that's the primary use case (fetch catalog before opening any route).

TESTS (non-vacuous, follow the crate's existing integration-test patterns — they spawn a real daemon):
1. Real-daemon test: connect a consumer, call catalog_list() with NO routes open, assert the fake-aft-stub module appears with its ToolProvider role and at least one tool carrying a non-empty name + schema. (The existing tests show how to spawn subc-core + fake-aft-stub; reuse those helpers. Remember the env-leak class: tests must run under a clean env — the existing harness handles SUBC_MODULE_ID isolation; follow it.)
2. Deserialization unit test: a canned catalog.list JSON reply (copy a REAL shape from an existing golden/e2e fixture or capture one from the real daemon — do not invent field names) decodes into CatalogList; unknown extra fields tolerated.
3. Deadline test: catalog_list against a consumer whose connection is down and cannot reconnect within a short deadline → NotSent-class error within the bound (mirror the existing deadline-bounded pre-send tests).

GREEN BAR: env -u SUBC_MODULE_ID -u SUBC_LAUNCH_NONCE cargo test -p subc-client-rs green; cargo clippy -p subc-client-rs --all-targets clean AND --target x86_64-pc-windows-gnu --all-targets clean; cargo fmt -p subc-client-rs -- --check; check_comments clean (comments explain the surface for a no-context reader, no task refs).

REPORT: API as landed, the real-daemon test evidence (which module/tool asserted), files changed, commit SHA.