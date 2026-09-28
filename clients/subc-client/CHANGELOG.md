# Changelog

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
