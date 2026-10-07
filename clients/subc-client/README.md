# @cortexkit/subc-client

TypeScript client for the [subc](../../README.md) daemon. It speaks the same
loopback-TCP transport as the Rust consumers (`subc-core`'s `subc-probe`),
wire-compatible **byte-for-byte** with `subc-transport` (the HMAC-SHA256
handshake) and `subc-protocol` (the 21-byte v2 envelope and channel-0 control RPCs).

Use it when a TypeScript/JavaScript process needs to reach a subc-routed module
(e.g. a provider module exposing a tool surface or a management surface).

## Install

It ships as source (no build step) and runs on Bun or Node ≥ 18 — the only
imports are `node:net`, `node:crypto`, and `node:fs`.

```jsonc
// from the subconscious monorepo
"dependencies": { "@cortexkit/subc-client": "workspace:*" }
```

## Usage

A consumer authenticates, optionally lists the catalog, opens a route to a
target module, then issues requests using the returned immutable route handle. There is no
client `HELLO` — `HELLO` is module-registration only.

```ts
import { SubcClient } from "@cortexkit/subc-client";

// The daemon publishes its connection file at $XDG_RUNTIME_DIR/subc-connection.json.
const client = await SubcClient.connect({ connectionFile });

// Optional: discover what is registered.
const modules = await client.catalogList();

// Open a route to a management-surface module and call it.
const route = await client.routeOpen(
  { kind: "management_surface", module_id: "ai-provider-quota" },
  { project_root: process.cwd(), harness: "my-harness", session: "session-1" },
);

// request() resolves to the module's full Response body (the parsed JSON),
// NOT an unwrapped field. A module decides its own response envelope; this one
// wraps its array under `result`, so read `body.result`.
const body = await client.request(route, { method: "usage.get", params: {} });
const usage = body.result; // ProviderUsage[] for the ai-provider-quota module

await client.closeRoute(route);
client.close();
```

### v2 route-handle migration

Route identity is now an immutable `RouteHandle { channel, epoch }` bound to the
connection that opened it. `routeOpen()` returns a handle, and `request()`,
`subscribe()`, `routePoll()`, `cancel()`, `closeRoute()`, and
`closeRouteChannel()` all require that handle. A handle retained across reconnect
fails locally with `StaleRouteHandleError` and emits no frame. The former
`closeRoute(target, identity)` managed-cache operation is now
`closeManagedRoute(target, identity)`; no public operation accepts a bare channel.

`connect()` runs the full handshake before resolving: `ClientHello` →
verify the server's proof **and** the daemon id from the connection file →
`ClientAuth`. A wrong key, an impostor daemon, or a tampered connection file
fails loud with an `AuthError` rather than connecting insecurely.

### Routing notes

- **Correlation, not order.** Every request carries a correlation id; replies are
  matched by `(channel, epoch, corr)`, never by arrival order. subc may interleave a
  control reply ahead of another exchange's response on the same connection.
- **Priority.** Channel-0 control RPCs and data-plane requests are sent
  `Interactive`. `request()` accepts `{ priority, admissionClass, timeoutMs, onProgress }`;
  `onProgress` receives interim `Push`/`StreamData` frame bodies before the
  terminal reply.
- **Errors.** A module that returns a `FrameType::Error` frame surfaces as a
  thrown `SubcError` carrying the canonical `{ code, message }`.
- **Connection-file security.** On unix the file must be owner-only (`0600`); a
  group/world-readable file is rejected, because the key has effectively leaked.

### Scoped routes

Use a daemon advertising `scopes/v1` and pass a selector to `routeOpen()` or to
managed `call()` / `callBinary()` options:

```ts
const scope = {
  owner: { kind: "reserved", module_id: "session-owner" } as const,
  ref: "session-1",
  scopeEpoch: 1,
};
const route = await client.routeOpen(target, identity, { scope });
await client.call("provider", "echo", {}, { scope });
// Close only this scoped managed route, not the unscoped or another scope's route.
await client.closeManagedRoute(target, identity, { scope });
```

The daemon checks who the caller is from its own connection record, not from the request: the caller must be the module that registered the scope (its owner), or a module the owner listed as a carrier on that scope record.
Passing `scope` does not grant that authority: use the supervised caller's
`consumerIdentity` (or its default environment identity) as usual. `owner` uses
the existing `Principal` type; the reserved form above is the normal case, and
the daemon refuses other owner kinds.

The exact wire selector is
`scope: { owner: { kind: "reserved", module_id: "session-owner" }, ref: "session-1", scope_epoch: 1 }`.
With no selector, there is no `scope` key. A reserved owner's module id and the
ref must be non-empty strings, and `scopeEpoch` must be present and a safe
non-negative integer (zero is valid); invalid selectors fail locally before I/O.

Managed routes are cached separately by target, bind and consumer identities,
reverse capabilities, and the whole scope selector (owner, ref, epoch). Scoped
and unscoped calls never share a route, nor do different scopes or daemon
incarnations. Reconnect opens a fresh handle under the same selector.
`scope_not_synced` and `scope_changed` refusals retain the managed path's retries
within the caller's deadline. A scope-revocation close (such as `scope_ended` or
`scope_carrier_removed`) is terminal: neither a later managed call nor a
reconnect automatically reopens that cache entry. Use a new selector for a new
session; reload and restart close reasons still permit reopens.

### Launch nonce

A module the daemon spawns proves who it is with a launch nonce. On macOS and
Linux the daemon hands it over as a pipe at descriptor 3, named by
`SUBC_LAUNCH_NONCE_FD=3:<inode>`, and during the rollout also as
`SUBC_LAUNCH_NONCE` (which any same-user process can read with `ps eww`).
`launchNonce()` is the one reader: it takes the descriptor only when it is a
pipe with the named inode that holds bytes, reads it to end of file, closes it,
and caches the value and its source (`fd` or `env`) for the life of the
process. A named descriptor that is closed, not a pipe, a different pipe, empty
or malformed throws a `LaunchNonceError` (`NotOpen`, `NotAPipe`, `WrongPipe`,
`Empty`, `Malformed`), leaves the descriptor as it was, and never falls back to
the environment copy. The environment copy is read only when
`SUBC_LAUNCH_NONCE_FD` is absent. `process.env` is never modified.

- `SubcProvider.connect()` sends the nonce in HELLO and rejects with a
  `SubcProviderError` coded `launch_nonce_unavailable` on a refusal. A declared
  `manifest.provenance` gets `launch_nonce_source` filled in when unset.
- `route.open` presents the nonce as consumer identity; a refusal means no
  identity.
- Call `launchNonce()` (or connect the provider) before spawning anything: until
  the first read, descriptor 3 is inheritable. A process spawned afterwards
  inherits the variable but not the pipe, and its accessor refuses by name.
- Every copy of this package in a JavaScript realm shares the cache. A worker
  thread is a separate realm, so read the nonce on the main thread.
- It relies on the write end of the pipe being closed before the module
  starts, as the daemon's handoff does. Node and Bun cannot count the bytes
  waiting in a pipe without reading them, so emptiness is checked with a first
  read, which returns nothing and consumes nothing at end of file; if a writer
  still held the pipe open, that read would block instead of reporting
  `Empty`. (fstat's size is no substitute: macOS reports the waiting bytes for
  an anonymous pipe, but 0 for a mkfifo pipe, and Linux reports 0 for every
  pipe.)

## Testing

```sh
bun run typecheck
bun test          # unit/mock suite; live tests are gated by RUN_SUBC_LIVE=1
RUN_SUBC_LIVE=1 bun test tests/live-scoped-open.test.ts
```

The live-handshake tests boot the real daemon binary
(`target/debug/ck-subc`) and complete the handshake against it — the
byte-identity authority for this client. They skip automatically when the binary
is not built; run `cargo build -p subc-core` first (the CI lane does this; the
package builds the `ck-subc` executable).

The scope admission test uses the same supervised `fake-aft-stub` fixture as the
Rust SDK (built by `cargo build -p subc-core --bins`) to sync an owner's scope and
observe the real provider bind. Live helpers hard-link the daemon as
`ckdev-subc` inside the scratch runtime directory before spawning it, and isolate
all three XDG homes so tests cannot reach the operator's daemon data or config.

## Layout

| File | Responsibility |
| --- | --- |
| `src/envelope.ts` | 21-byte header codec, frame types, flags, priority, admission class |
| `src/connection-file.ts` | read + validate the daemon connection file (owner-only gate) |
| `src/socket.ts` | prefix-first envelope reader and deadline-bounded buffered TCP I/O |
| `src/auth.ts` | HMAC-SHA256 handshake (`computeProof`, constant-time verify) |
| `src/route-handle.ts` | immutable connection-bound `(channel, epoch)` route identity |
| `src/client.ts` | `SubcClient`: route handles, channel-0 RPCs, epoch-aware corr-mux |
| `src/launch-nonce.ts` | the one launch-nonce reader: descriptor 3 or the environment copy, cached |
| `src/provider.ts` | provider bind publication, epoch validation, and routed serving |
