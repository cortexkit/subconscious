# Build: daemon self-watchdog (subc health spec §8)

Implement §8 of `docs/specs/subc-health.md` (v2) in `crates/subc-core`. Small,
bounded task. The spec is normative; STOP and ask on genuine contradiction.

## Scope

A watchdog task the daemon spawns after bootstrap:

1. Every 60s (const, jittered ±10% like the prober), open a loopback client
   connection to the daemon's OWN published endpoint using its OWN key
   (the daemon authenticates to itself; accept spawns per-connection tasks so
   this cannot deadlock the accept loop), drive a channel-0 `server.describe`,
   verify a well-formed reply, close cleanly.
2. SCOPE per spec: this proves accept + auth + control dispatch ONLY (not the
   data-plane forwarding path — explicitly out of v1).
3. Connection-file integrity rides the same tick: file present, owner-only
   permissions (unix), parses as ConnectionInfo, matches the live port and
   key. Any mismatch = ERROR log naming exactly what diverged.
4. A failed tick (connect/auth/describe error or timeout — 5s deadline) logs
   ERROR with the failure stage and diagnostics. Consecutive-failure count
   included in the log line. NO self-restart action in v1 (launchd KeepAlive
   is the outer layer; the watchdog is a loud signal, not an actor).
5. Healthy ticks are silent (no INFO spam); log one INFO line on recovery
   (failure streak ended) with the streak length.

## Wiring

- Spawn from the bootstrap path after listeners are serving (it needs the
  connection file already published). AbortOnDrop pattern like the existing
  spawned tasks; clean shutdown with the daemon.
- The loopback client can reuse crates-internal transport helpers
  (authenticate_client from subc-transport + read/write_frame) — do NOT add a
  dependency on subc-client-rs.
- Tick interval/deadline as consts with a test override (injectable via the
  same pattern other timeouts use, e.g. a with_ builder or env — follow the
  existing ROUTE_BIND_RELAY_TIMEOUT injectability precedent).

## Tests

- Integration (existing TestServer harness): watchdog tick against a live
  daemon succeeds silently; corrupt/replace the connection file → ERROR
  logged naming the divergence (use the tracing test-capture pattern if one
  exists in the repo, else assert via a observable counter surfaced on the
  watchdog struct for tests).
- Failed-tick path: point a watchdog at a dead endpoint → ERROR with stage,
  consecutive count increments; recovery logs INFO with streak.
- Level-triggered polling discipline, injected small intervals, no absolute
  latency assertions (repo norm).

## Definition of done

Workspace cargo test green; clippy -D warnings native + x86_64-pc-windows-gnu;
fmt; comments carry reasons only; logical commits.
