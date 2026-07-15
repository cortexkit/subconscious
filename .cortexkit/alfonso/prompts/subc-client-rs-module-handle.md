# Build: subc-client-rs module-side ModuleHandle with catalog_update

## Why

P1 `catalog.update` shipped in subc-protocol/subc-core (module-origin channel-0
op: `ModuleControlRequestFromModule::CatalogUpdate` → ack
`ModuleControlResponseToModule::CatalogUpdate {}`), but the serve SDK gives a
module no way to SEND it: `serve`/`serve_with` in
`crates/subc-client-rs/src/lib.rs` own the connection inside `module_loop` and
return no handle. The federation module needs to emit catalog.update on its
provider connections for in-place catalog refresh (no reconnect churn). This is
a general SDK gap, not federation-specific.

## What to build

In `crates/subc-client-rs` (module/serve side, NOT the consumer):

1. A handle-returning serve variant. Naming/shape guidance (adapt if the
   existing internals suggest better, but keep an OWNED handle + the serve
   future):

   ```rust
   pub async fn serve_with_handle(
       connection_file: &Path,
       manifest: ModuleManifest,
       handler: impl ModuleHandler,
   ) -> Result<(ModuleHandle, impl Future<Output = Result<(), SubcModuleError>>), SubcModuleError>
   ```

   The future is the existing serve loop; the handle is Clone + Send + Sync.
   Existing `serve`/`serve_with` keep their signatures (implement them over the
   new variant internally so there is one loop, not two).

2. `ModuleHandle::catalog_update(&self, provides: Vec<ProviderRole>) -> Result<(), CatalogUpdateError>`:
   - Sends a channel-0 Request frame with body
     `ModuleControlRequestFromModule::CatalogUpdate { provides }` (op
     `catalog.update`), corr allocated from the same space as other
     module-originated requests (check how health replies/bind acks are
     corr-managed in module_loop and integrate with that demux — the reply is a
     Response frame on channel 0 with our corr; body
     `ModuleControlResponseToModule::CatalogUpdate {}`).
   - Awaits the ack with a bounded timeout (10s default).
   - Error surface: typed — daemon Error frame with code
     `catalog_update_frozen_field` / `not_registered` must map to distinct
     variants (callers branch on frozen-field), plus Timeout and
     ConnectionClosed.
   - Concurrency: multiple in-flight catalog_updates from clones are legal
     (corr-demuxed); do not serialize behind a lock across the await.

3. Capability note: HELLO_ACK `subc_ops` advertises `catalog.update` (shipped).
   The handle should check the stored ack's subc_ops and fail fast with a
   typed `NotSupported` error when absent (old daemon), rather than timing out.

## Tests

Real-daemon integration tests (existing patterns in crates/subc-client-rs/tests/):
- serve_with_handle + catalog_update happy path: register with tools [a,b],
  handle.catalog_update([a,c]), assert ack Ok, then a consumer's catalog.list
  reflects [a,c] and an open route on the module SURVIVES the update (bind
  before update, call after update succeeds).
- frozen-field rejection: update attempting empty provides (routability
  boundary) → typed FrozenField error variant.
- old-daemon guard: strip catalog.update from the ack's subc_ops (or simulate)
  → NotSupported without any frame sent (assert no frame, not just error).
- handle after connection death → ConnectionClosed (not hang).

## Gates

cargo test -p subc-client-rs (env -u SUBC_MODULE_ID -u SUBC_LAUNCH_NONCE),
clippy -D warnings native + x86_64-pc-windows-gnu, fmt, check_comments.
No changes outside crates/subc-client-rs unless a genuine protocol gap is
found (there should be none — the wire shipped in 2066e4b3; if you find one,
STOP and report rather than patching subc-core).
