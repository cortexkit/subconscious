# Changelog

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
