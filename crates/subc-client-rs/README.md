# subc-client-rs

> [!WARNING]
> **Alpha.** subc-client-rs is in early development and is not yet released for general use.
> Breaking changes will happen between versions, including changes to its settings,
> stored data and interfaces. Pin an exact version if you depend on it.

Opt-in modules can use the `config` module's documented `ModuleHandler` hooks to
declare, review and apply daemon-provided configuration. Its compile-checked
counter sample shows the complete adoption in fewer than 60 lines. Gate refusals
never authorize reading local files; `config_files` runs only with an older daemon
whose HELLO_ACK lacks `config.gate`.
View statuses other than `active`, `pending` or `unstamped` are reported through `on_config_error` as protocol errors and are never applied or acknowledged.

Rust module serving and consumer APIs for the Subconscious daemon.

## Custom module loops and registration-channel identity

`serve` and `serve_with_handle` adopt the daemon's private registration channel
automatically. A module that owns its loop can use `registration` directly:

1. Call `registration::adopt_channel()` once at startup, in a Tokio runtime and
   before spawning children. `None` means no validated channel is available;
   continue with the existing connection-file and launch-nonce path.
2. Authenticate the returned stream using the connection file's handshake inputs.
3. Send the launched module's HELLO **without `launch_nonce`** and validate the
   daemon's HELLO_ACK.
4. Call `registration::install_claim_minter(Arc<dyn registration::ClaimMinter>)`.
   Implement the trait with `#[subc_client_rs::async_trait]`: send its existing
   `subc_protocol::claim::ClaimMintRequest` over channel 0 of the same registration
   connection, correlate/validate the response, and return `ClaimMintReply` or
   `ClaimError`. Keep reading the connection while minting, honor bounded
   rate-limit retries, and fail closed when it ends; never reconnect it.
5. Only then open `SubcConsumer` connections. Each presents a fresh self claim,
   just as consumers opened under `serve` do.

Channel adoption (even when it returns `None`) and owner installation are each single-use.
Installing before adoption or installing twice refuses. Without an installed
minter, a consumer in an adopted process fails closed, never falling back to a
nonce. Do not pass `CallOptions.consumer_identity` on this path: a claim-bound
connection refuses the override before sending it.

## Slow startup without blocking the daemon connection

Register a supervised module with `ModuleManifest::builder(...).ready(false)`.
The daemon refuses new routes with the retryable `module_warming` error until
the module reports readiness. Start the serving future immediately: it must keep
reading the connection to answer `health.check`, `route.bind` and stop notices.
Do slow initialization outside that reader task, not in `on_hello_ack`.

`serve_from_env_with_handle` uses the supervisor's normal launch inputs and returns
a cloneable `ModuleHandle` plus the serving future. For an explicit connection
file, use `serve_with_handle`. After startup, call
`ModuleHandle::set_ready(true).await`; setting false later refuses new binds but
does not interrupt existing routes. Readiness updates require **subc-daemon
0.20.3 or later** (including subc-daemon 0.38.x).

```rust,ignore
let manifest = ModuleManifest::builder("my-module", env!("CARGO_PKG_VERSION"))
    .provides(provider_roles)
    .ready(false)
    .build();
let (module, serving) = serve_from_env_with_handle(manifest, handler).await?;
let serving = tokio::spawn(serving);

// Startup runs beside the reader, and stops if the connection ends.
tokio::select! {
    result = initialize() => result?,
    () = module.closed() => return Ok(()),
}
module.set_ready(true).await?;
serving.await??;
```

Give a clone of the returned handle to handler shared state before spawning the
serving future if startup or drain hooks need it. Inside a request handler,
`ctx.module_handle()` returns the same connection's handle. Existing
`ModuleStopHandle`, `ServeOptions` and drain hooks are unchanged; keep polling
the serving future during shutdown so it can drain and finish teardown.

`set_ready` resends the current provider roles unchanged and omits capabilities
to preserve the previous declaration. Successful `catalog_update` and
`catalog_update_with_capabilities` calls replace the remembered roles. Await a
catalog replacement before changing readiness: an in-flight replacement makes
the current roles unknowable, so `set_ready` returns
`CatalogUpdateError::Protocol` without sending. A cancelled or timed-out update,
or an invalid reply, also leaves the roles uncertain; a successful explicit
catalog replacement establishes them again. A typed daemon rejection preserves
the previously accepted roles. The ordinary catalog methods still omit `ready`
and leave readiness unchanged.

## Scope lifetime watches

Use `ModuleHandle::scope_watch(Vec<ScopeWatchSelector>)` on the module's
registered connection, beside `scope_describe` and `scope_apply`. It checks the
`scope.watch` op advertised in HELLO_ACK; an older daemon returns
`ScopeCallError::NotSupported` without sending a request. A separate consumer
connection cannot watch scopes because it cannot describe them.

The returned `ScopeSubscription.initial.scopes` contains one state per selected
`ScopeRef { owner, scope_ref }` and epoch (`scope_ref` serializes as `ref`). States
are `Live`, `Ended`, `Superseded` with `by_epoch`, or `UnknownScope`. Then
`next().await` returns typed `ScopeWatchPush` events until cancellation,
connection loss or a terminal daemon refusal. Queued events precede the terminal
error, and refusal codes remain in `ScopeCallError::Refused`.

A watch holds at most 256 selectors, with at most 16 watches per connection.
Drop or `unsubscribe()` to cancel, and reopen to replace the set. After
re-registering on a new connection, open a fresh watch and inspect its initial
states; unknown state and a broken feed never prove a scope ended. See
[scope lifecycle rules](../../docs/scopes.md#lifecycle-scopewatch).
