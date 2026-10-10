# Changelog

## 0.21.2

- Measure a frame body's read budget as time without progress rather than wall-clock time since its header. Each incoming chunk pushes the deadline out by the budget again, so a slow but moving body does not time out, while a body that stops arriving still fails after the budget. Absolute deadlines, such as the handshake's, stay fixed: arriving bytes never extend them.
- Defer deadline timeout rejection past the event loop's I/O poll phase (`setImmediate`) before failing. In Node and Bun, expired timers run before I/O polling, so if the host process's event loop was blocked by CPU-bound work or a busy thread, an armed read deadline timer could expire while incoming bytes were already sitting in the OS kernel socket receive buffer. When the event loop resumed, the timer fired before the socket's data handler could read those bytes, causing parallel calls sharing the connection to fail together with "timed out waiting for N bytes" despite no network or daemon stall. The deferred check allows pending kernel bytes to be delivered and processed before any timeout is raised.
- Add optional `bodyReadTimeoutMs` to `ConnectOptions` and `SubcProviderConnectOptions` to make the body read budget injectable.

## 0.21.1

- A cached managed route now ends permanently only when its scope ends. Other route closes, including crashes and connection loss, discard the cached route so the next call opens a fresh one; calls already in flight still fail without being resent. This fixes a regression in 0.21.0, where a long-lived client refused every later call to a module after that module crashed and was restarted.

## 0.21.0

- Classify `target_flow_unsupported` as a terminal route-open refusal, matching the golden decision table in subc-protocol 0.29.1. A target must declare `flow-scopes/v1` before it may receive a flow-scoped route.
- Add optional `scope: { owner: Principal, ref: string, scopeEpoch: number }` to direct route opens, managed calls (including binary calls), and managed closes. The selector is sent as `scope: { owner, ref, scope_epoch }`, matching the Rust protocol. Owner and ref are validated locally, and a missing, negative, fractional or unsafe epoch is refused before sending.
- Isolate managed route cache entries by the entire scope selector, apart from unscoped routes. Reconnect reopens retain the selector but never reuse a handle from an earlier connection or daemon incarnation.
- Preserve deadline-bounded retries for `scope_not_synced` and `scope_changed`. Terminal lifecycle close reasons, including scope revocation, prevent automatic managed reopens under the same selector; restart and reload still allow them.
- Scoped opens require the caller to be the scope's owner or a listed carrier. The normal owner form is a reserved principal; the daemon refuses other owner kinds.

## 0.20.1

- Queue unary and managed REQUEST frames before attaching cancellation signals so an already-aborted signal cannot send CANCEL before its REQUEST.
- On provider close, daemon GOODBYE, or fatal disconnect, reject pending reverse requests immediately, abort handler signals, and invalidate installed routes. Explicit close rejects with `provider_closed`, daemon GOODBYE with `connection_closed`, and fatal failures retain their cause.
- Match Rust and Swift header-validation precedence: a pure-header control frame with both a body and a nonzero epoch reports `pure_header_frame_with_body` before `nonzero_epoch_on_control_channel`.
- `readConnectionFile` refuses a connection file owned by any uid other than the process's effective uid, even at mode 0600, because its owner could have written their own endpoint and key. The file is opened once and the ownership and permission checks run on the opened file (fstat), so replacing the path between the check and the read cannot slip an unchecked key through. The refusal is a `ConnectionFileError`. The owner check is skipped where `process.geteuid` does not exist (Windows), matching subc-transport.
- `encodeFrame` refuses, before producing any bytes, a frame whose header the wire decoder would reject (it runs the encoded header through `decodeHeader`) and a body over `MAX_FRAME_BODY_LEN`, throwing `DecodeError` with the decoder's code (`frame_body_too_large` for the size). Every frame the SDK writes passes through it, so an invalid hand-built frame can no longer reach the socket and make the daemon drop a healthy connection. Frames built with `buildFrame` were already validated and are unaffected.

## 0.20.0 — 2026-10-02

- Export `RouteEndReason` and expose `closeReason` on `SubcError` and `SubcCallError`. Named channel reasons take precedence over legacy module-only pushes; caller closes and connection losses report SDK-side reasons. Channel reuse clears history and call retry kinds are unchanged.
- Decode all four scope close reasons as known, non-reopenable causes.

## 0.19.0 — 2026-09-30

- Add optional `onDraining(reason: RouteCloseReason, deadline: Date)` to `SubcProviderConnectOptions`. Channel-0 `module.draining` Push notices start the callback without waiting for completion, so PING and GOODBYE keep flowing. GOODBYE and connection-end reporting wait at most 2 seconds for hooks to start. Throws and rejections are contained; unknown reasons arrive as `unknown`. Undecodable channel-0 Push frames are ignored with one warning per connection.
- Document the wall-clock deadline as a no-later-than bound. To keep draining open for background work, declare a Busy self-signal in the manifest that names health gauges (work counters), and report them above zero in health metrics until work finishes; the daemon waits for those counters to reach zero or for its deadline.

## 0.18.1 — 2026-09-30

- `isRetryableRouteOpenCode` treats `scope_not_synced` and `scope_changed` as retryable, following the shared `decision_tables.json` record. The daemon returns them for scoped `route.open` requests: the first when the scope's owner has not re-synced since a daemon restart, the second when the scope changed between admission and commit. In both cases nothing was sent. Daemons that do not support scopes never return either code.

## 0.18.0 — 2026-09-30

- Add `launchNonce()`, the one reader of the launch nonce, matching `subc_os::launch_nonce` in Rust. When `SUBC_LAUNCH_NONCE_FD=<fd>:<inode>` is set (not on Windows) it takes the named descriptor only if it is a pipe with that inode holding bytes, reads it to end of file and closes it; a closed descriptor, a non-pipe, a different pipe, an empty pipe or a malformed value throws `LaunchNonceError` (`kind` `NotOpen`, `NotAPipe`, `WrongPipe`, `Empty`, `Malformed`; also `Unreadable`, `NotUtf8`), leaves the descriptor untouched and never falls back to `SUBC_LAUNCH_NONCE`. Without the variable it reads `SUBC_LAUNCH_NONCE`. The answer and its source (`fd` or `env`) are cached for the process, shared by every copy of the package in the same realm, and `process.env` is never changed. Also exported: `launchNonceOrUndefined`, `isLaunchNonceError`, `SUBC_LAUNCH_NONCE_FD_ENV`, `LAUNCH_NONCE_FD` and the `LaunchNonce`, `LaunchNonceSource`, `LaunchNonceErrorKind` types.
- Unlike the Rust accessor, which counts the waiting bytes with FIONREAD, the emptiness check is a first read (Node and Bun expose no FIONREAD). At end of file it returns nothing and consumes nothing, so an empty pipe is still refused untouched; but if the pipe's write end were still open somewhere, an empty blocking pipe would block that read instead of reporting `Empty`. The daemon closes the write end before spawning the module.
- `SubcProvider.connect()` reads the HELLO nonce through the accessor. A refused descriptor rejects `connect()` with a `SubcProviderError` coded `launch_nonce_unavailable` (`detail.kind` and `cause` carry the accessor error, the message matches the Rust SDK's) before anything is sent. A provider counts as supervised when `SUBC_MODULE_ID` is set and the accessor holds a nonce.
- `route.open` takes its consumer identity nonce from the accessor; a refused descriptor opens the route without identity.
- `ManifestInput` gains optional `provenance` (`ManifestProvenance`, mirroring subc-protocol), sent in HELLO only when declared. When declared without `launch_nonce_source`, the provider fills it with the accessor's source if the accessor holds the nonce HELLO sends.
- Behaviour change for tools a module spawns: once the module has read the descriptor, a process it spawns inherits `SUBC_LAUNCH_NONCE_FD` without the pipe, so its accessor refuses (`NotOpen`, `NotAPipe` or `WrongPipe`) instead of silently acting as the module with the inherited `SUBC_LAUNCH_NONCE`.

## 0.17.0 — 2026-09-28

- Managed calls now keep retrying a retryable `route.open` refusal for up to 90s instead of 30s (`ROUTE_OPEN_RETRY_DEADLINE_MS`). A module restart drains its routes (up to 30s), waits for the old process to stop, then boots the new one; one measured restart kept routes refused for 62.5s, which the old deadline gave up on. A call that passes `timeoutMs` stops retrying at that timeout when it comes first, and a call waiting on another call's in-flight open of the same route also stops at its own `timeoutMs`.
- Retry delays between refused `route.open` attempts are jittered ("equal jitter": half the delay is kept, half is random), so routes refused together no longer retry in lock step. The new `random` connect option injects the random source and `now` injects the clock for tests. Reconnect backoff is unchanged.
- At most `MAX_ROUTE_OPENS_IN_FLIGHT` (8) `route.open` requests are outstanding per connection, matching the daemon's per-connection limit. Further opens wait their turn in order, and a managed open's wait counts against its retry deadline. Before, a burst of opens after a restart exceeded the limit and the daemon refused the excess.
- When the retry deadline runs out, the error message now describes the most informative refusal seen during the retries (`module_reloading` / `module_warming` first, then `delegation_not_registered`, then admission pressure such as "8 binds in flight", then anything else), including its `detail.reason`, and names the most recent refusal after it when that differs. The error's `kind` (`not_sent`), `code` and `cause` are still those of the most recent refusal.
- Export `isRetryableRouteOpenCode` from the package entry, so consumers with their own retry loop share the classification instead of copying it.

## 0.16.1 — 2026-09-24

- Export `UNKNOWN_CHANNEL`, `STALE_ROUTE_EPOCH`, and `isEstablishedRouteDead(flags, code)` for evict/reopen/resend-once decisions. The managed consumer and provider use the shared predicate without changing their current retry or `not_sent` behavior. The golden table records daemon-origin flags but does not enforce them until all compatible daemons emit the bit.

## 0.16.0 — 2026-09-24

- Make `SubcProvider.closed` public: a `Promise<void>` that resolves once, when the provider will serve no more. That happens after a channel-0 GOODBYE from the daemon (supervisor restart or stop, daemon shutdown), after `close()`, or after a connection loss the provider will not recover from. It never rejects. A supervised module should `await provider.closed` and then exit (`process.exit(0)`), so a restart no longer waits for the daemon to SIGTERM it at the drain deadline. The SDK itself never exits the process.
- Add the `reconnectOnDrop` connect option. A supervised provider now ends serving and resolves `closed` when its connection drops, instead of reconnecting and re-registering, because the daemon owns its restarts. A provider counts as supervised when both `SUBC_MODULE_ID` and `SUBC_LAUNCH_NONCE` are set and non-empty. Unsupervised providers keep reconnecting as before. Pass `reconnectOnDrop: true` or `false` to override either default.
- The provider now reads `SUBC_LAUNCH_NONCE` once, at `connect()`, instead of on every HELLO. Re-registration after a reconnect echoes the same nonce.

## 0.15.0 — 2026-09-23

- Expose the daemon's machine id on `SubcProvider.machineId`, read from the new optional `machine_id` field of `HELLO_ACK` and refreshed on every re-registration, in the same way as `storage`. It is `undefined` when the daemon predates the machine id or sends a value that is not 32 lowercase hex characters; the client never mints a substitute. The id names a machine and is never an authority. `machineIdFromHelloAck` exposes the same validation.
- Live tests now give the spawned daemon its own `XDG_DATA_HOME`, so they never mint a machine id into the operator's real data home.

## 0.14.0 — 2026-09-23

- Make `unknown_module` a terminal `route.open` refusal, following the shared `decision_tables.json` record. The daemon now reports a configured-but-late target as `module_warming` or `target_unavailable` (both still retried), so what remains under `unknown_module` — a typo'd id or a peer not deployed on this host — no longer enters the managed retry loop and fails immediately instead of after the retry deadline. Callers that open a route before an unsupervised module's HELLO lands now own that retry themselves.

## 0.13.3 — 2026-09-23

- Fix an AbortSignal listener leak: a request that settled before its signal fired left its abort listener attached, so a long-lived signal reused across calls accumulated one closure per request. The listener is now removed when the request settles.
- Isolate a throwing `onRouteGone` callback: it is now reported via `console.warn` and absorbed instead of escaping the read loop, where it was treated as an unexpected drop and tore down every route on the provider's connection.
- Isolate a throwing `onBound` callback the same way. The route stays bound, since the daemon already considers it live, and the consumer can close it.

## 0.11.1 — 2026-09-05

- Add per-route fault isolation during reconnect reopen, so one refused route no longer fails waiting calls on other routes.

## 0.11.0 — 2026-09-04

- Add opaque binary request bodies and wire-flag-driven binary replies.
- Add `callBinary()` for managed routes while keeping `call()` JSON-only.

## 0.8.2 — 2026-08-24

- Add capability-addressed provider resolution from the catalog capabilities mirror.
- Add deterministic plural resolution, singular ambiguity/unprovided errors, and local identifier validation.
