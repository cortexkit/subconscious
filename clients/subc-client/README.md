# @cortexkit/subc-client

TypeScript client for the [subc](../../README.md) daemon. It speaks the same
loopback-TCP transport by default, with an opt-in same-user local socket or pipe,
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

To use the daemon's Unix socket (macOS/Linux) or named pipe (Windows), opt in
when connecting:

```ts
const client = await SubcClient.connect({ connectionFile, transport: "local" });
```

Every connection attempt, including each reconnect, reads `local_endpoint` from
the current owner-checked connection file. An absent key gives
`local_endpoint_absent`; a failed local connect preserves its error (as the
`cause` of `DaemonUnavailableError` for connection-unavailable failures). Local
mode never falls back to TCP. Both transports use the same handshake and frames.
Providers registering modules still use TCP.

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

### Unavailable daemon

Initial `ENOENT` or `ECONNREFUSED` failures and exhausted transient transport
reconnects throw `DaemonUnavailableError` (a `SubcCallError`). Its
`recovery: "user_action_required"` tells hosts to stop issuing CortexKit tool
calls and ask the user to run `ck setup` or restart the CortexKit service. Resume
only after the user confirms it is running. `code` retains the original error
code when present, and `cause` retains the original error for diagnostics.

The recovery classification is separate from `kind: "not_sent"`, which only
describes whether request bytes left the process. Stale-route refreshes do not
require user action, and `outcome_unknown` still means an already-sent request
may have executed. The existing bounded reconnect budget still handles brief
daemon restarts before this error is surfaced.

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

### Owning scopes

A supervised, registered provider can publish scopes for its current launch.
Configure the owner as a reserved module in the daemon and run this with Bun:

```ts
import { managementSurfaceManifest, SubcProvider } from "@cortexkit/subc-client";

const provider = await SubcProvider.connect({
  connectionFile: process.env.SUBC_CONNECTION_FILE!,
  manifest: managementSurfaceManifest({
    moduleId: process.env.SUBC_MODULE_ID!,
    operations: [],
  }),
  handler: async (_route, body) => body,
});
const reply = await provider.scopeSync(1, [{
  scope_ref: "rig-session",
  scope_epoch: 1,
  kind: "head",
  workdir: process.cwd(),
  carriers: [{ principal: { kind: "reserved", module_id: "test-client" } }],
}]);
if (reply.results.some((result) => result.outcome === "refused")) {
  throw new Error(JSON.stringify(reply.results));
}
await provider.closed;
```

Set `SUBC_CONNECTION_FILE` to the daemon's connection file in the owner's
configured environment. The daemon supplies `SUBC_MODULE_ID` and its launch nonce.

`scopeSync(generation, scopes): Promise<ScopeSyncReply>` replaces the owner's
whole set, ending omitted scopes. `scopeApply(generation, upsert, end):
Promise<ScopeApplyReply>` changes only named scopes, after a successful full
sync on the same connection. Both operations share a strictly increasing
generation sequence. After reconnect, sync the full set again before applying
deltas. To end one scope, use
`await provider.scopeApply(2, [], [{ scope_ref: "rig-session", scope_epoch: 1 }])`.

`ScopeRecord` uses Rust input names: `scope_ref` (sent as `ref`), `scope_epoch`,
`kind`, `parent`, `child_owners`, `carriers`, `expires_at_ms`, `workdir`,
`project_id`, `bind_proof_sha256`, `replace_only_if_idle`, and `attributes`.
Parents and end inputs also use `scope_ref`. Attributes include `agent_id`,
`on_behalf_of_agent`, `delegates`, the flow identity fields and `run_id`; only
owners in `scope_authority_owners` may set nonempty attribution. Unknown nested
input fields are rejected, not silently dropped. Numeric inputs must be safe
non-negative integers. Optional null values, empty default lists, false flags
and empty attributes are omitted exactly as in Rust.

Replies retain the daemon's `ref` field names. Inspect `results` for each
upsert's outcome and refusal `code`/`message`, `end_results` for explicit ends,
and `ended` for ended scopes (normalized to `[]` when absent). Whole-call
refusals throw `SubcProviderError` with the daemon's code; unsupported operations
fail locally with `not_supported`. Calls time out after ten seconds; a timeout
does not prove the mutation was unapplied. Workdirs must be absolute and are
canonicalized by the daemon. A target providing `scope-workdir/v1` receives the
directory as `workdir` in the bind's `scope`. A carrier uses the selector shown above
with its own supervised consumer identity; the selector alone grants nothing.

### Opt-in configuration delivery

Pass `config: ModuleConfigOptions` to `SubcProvider.connect()` to list
`config.review` in HELLO `control_ops`. It does not change the manifest.
`provider.subcCapabilities` retains the current HELLO_ACK capabilities, including
`config.gate`; reconnect replaces them and sends one new declaration. A provider
without `config` keeps its existing behavior.

The callbacks separate these responsibilities:

- `declaration` supplies the schema, `user_file` and `project_dirs`.
- `review(request)` is pure: use only the supplied immutable layer snapshots and
  active `base`. Do not read files, environment, network or secrets. Return the
  normalized field changes (including sources/apply kinds) and any refusals.
- `apply(view)` installs the whole supplied view before returning `{result:"ack"}`.
  Return `{result:"nack", reasons:[...]}` only after keeping/restoring the last
  view. Apply `pending` rather than reapplying `active` when both are supplied.
  The SDK reports the exact generation, project and delivery attempt.
- `readFiles()` returns a generation-zero `unstamped` view using the old file
  reader. It runs **only** when HELLO_ACK lacks `config.gate`. A gate refusal
  never calls it. Unstamped views have no delivery attempt to ACK on the wire.

`configLayerBytes(snapshot)` verifies canonical base64 and the raw SHA-256 before
decoding bytes. Missing files return `undefined`, not empty bytes. Paths in a
snapshot are identities, never instructions to open a file. The SDK does not
compute canonical configuration or approval digests; echo daemon-provided ones.

`provider.config.state` and `onState` expose these discriminants:

| Wire status/code | `state` | Meaning |
| --- | --- | --- |
| `unstamped` | `unstamped` | Current file view, applied locally |
| `pending` | `pending` | View applied locally, outcome not yet accepted |
| `active` | `active` | Active view or accepted ACK |
| `failed` | `failed` | NACK/failure; previous view retained if present |
| `config_gated` | `config_gated` | Refusal plus `availability: "last_view"` and `view`, or `availability: "no_approved_config"` with no bytes |
| `not_delivery_target` | `not_delivery_target` | Keep the view, never mark gated; wait for a hint after cutover |
| `anchor_pending` | `anchor_pending` | Keep the view/outcome and retry on a hint or bounded backoff |
| Unknown string | `unknown` with `rawState` | Preserve the spelling; never activate it |

Refusal states include the original error in `refusal.cause`. None ends serving
or exits the process. `onError` reports unexpected transport/protocol/callback
failures without falling back to files. `config.changed` resynchronizes without
blocking the frame reader; `provider.config.refresh()` allows an explicit refresh.
Anchor retry intervals default to 1000 ms and must be 1..30000 ms. Closing the
provider cancels retries. Plain unstamped file edits currently require explicit
refresh; changed-hint handling is covered separately by wire tests.

See the paired [legacy sample](tests/fixtures/config-sample-legacy.ts) and
[adopting sample](tests/fixtures/config-sample.ts), whose line diff is checked to
stay within 60 changed lines. Run its real-daemon test on Linux after prebuilding
`cargo build --locked -p subc-core --bin ck-subc`:

```sh
RUN_SUBC_CONFIG_LIVE=1 bun test tests/live-config.test.ts
```

That test isolates HOME, the three XDG homes and the connection file. It covers
unstamped declare/get/local apply and explicit refresh, plus the unchanged
non-opted sample. Stamped N+1 delivery, review, NACK, hint handling, gate refusals,
no-approved-config availability, non-target replies and anchor retries are
deterministic wire tests using the committed Rust protocol goldens; they are not
claimed as stamped real-daemon coverage.

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
# Supervised registration channels

Modules using the registration channel declare `registration_channel: true` in
their manifest. The SDK validates and adopts the process's one inherited channel
at import, sets close-on-exec before children can be spawned, and caches the
decision. It uses that channel for HELLO and provider traffic, never reconnects
it (even with `reconnectOnDrop: true`), and exits on its EOF. Start the provider
before opening consumer connections: one process-wide broker mints fresh,
single-use `self` claims over that registration. Minting or presenting a claim
cannot fall back to `direct`; a refused presentation closes its connection.
Per-request consumer identity overrides are refused on a claim-bound connection.
Ordinary children inherit no channel and connect as `direct`, even if its public
environment metadata was inherited. Nonce-only providers still authenticate on the TCP listener and echo their launch nonce in HELLO.

## Native code and distribution

`clients/subc-registration-native` owns the N-API v8 addon. It shares socket,
close-on-exec and Windows pipe server-PID validation with `subc-os`. A plain
client never loads the addon. A process with registration metadata but without
the matching binary is not supervised; attempting provider registration refuses
and exits instead of falling back to the listener.

The SDK ships **six** native builds, under its `native/` directory:
`subc-registration-{linux,darwin,win32}-{x64,arm64}.node`, plus the native
package's loader and declarations. Selection uses the running process's platform
and architecture (including emulation). The loader only loads the adjacent file:
no environment override, system search, download or install-time compilation.
These binaries are loaded code and must be pinned with the SDK inside the
module's release directory, whose code the daemon verifies before launch (the
admitted bundle). Release assembly builds the six targets on their
native toolchains and collects them in `native/`; the prepublish check refuses an
incomplete six-target bundle. This does not add release CI or claim the unbuilt
targets were qualified.

For development and SDK tests, run `bun run build:native` in this package first.
It builds the new Rust crate in the normal Cargo target directory and stages the
host binary with the loader in `native/`. On Windows it builds the MSVC target
matching the Node process's architecture. Run `bun run typecheck` and
`bun test tests/claim.test.ts tests/registration-channel.test.ts`. In CI or remote
verification, build the addon in the same job that runs these tests. Artifacts
are not committed.

## Explicit helper handoff

Agent shells, tools and plugins get no claims. Only an explicitly requested
helper receives a `helper` claim, through a one-read inherited pipe (a
single-instance, PID-checked named pipe on Windows). Keep the normal
`ChildProcess` so stdio, exit events and cancellation remain available:

```ts
import { spawn } from "node:child_process";
import { once } from "node:events";
import { prepareHelperClaim } from "@cortexkit/subc-client";

const handoff = prepareHelperClaim(3);
const child = spawn(command, args, {
  env: { ...process.env, ...handoff.environment }, // public pipe metadata only
  stdio: handoff.childFd === null
    ? ["pipe", "pipe", "pipe"]
    : ["pipe", "pipe", "pipe", handoff.childFd],
});
child.once("error", () => handoff.abort());
try {
  await once(child, "spawn");
  await handoff.complete(child.pid!); // mints now, writes once to this child
} catch (error) {
  handoff.abort();
  throw error;
}
```

Keep the child alive until completion returns to prevent PID reuse. Abort on a
spawn error; abort/drop writes no claim. On Unix, only the stdio duplicate is
inheritable and completion closes the parent's copy of the child end before
writing. The helper's SDK presents the claim as the first post-handshake frame.
A helper needing another connection must request a new handoff from its owner;
there is no refresh token. Never put claims in argv, environment values or logs.
