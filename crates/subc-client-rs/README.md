# subc-client-rs

Rust module serving and consumer APIs for the Subconscious daemon.

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
