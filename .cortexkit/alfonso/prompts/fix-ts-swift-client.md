# subc clients (TS + Swift): unknown_channel cache hygiene + connection-file wire_version check

Two client-side fixes in the subconscious repo, from a root-cause audit after the wire-v2 flip. Do NOT touch crates/ except reading for reference — the Rust side is a separate task.

## 1. TS client: evict the cached managed-route handle on EVERY unknown_channel (clients/subc-client/src/client.ts)

Current behavior in the managed `call()` path: on the FIRST `unknown_channel` error the client nulls the cached handle and re-opens the route in place (good). But when the RETRY also fails with `unknown_channel`, the error is thrown WITHOUT evicting the retry's handle from the managed-route cache — the next `call()` first wastes a request on that known-dead cached handle, then evicts and rebinds. Under a module that lost its routes this sustains one extra dead request + one bind per call, forever.

Fix: on the second (retry) `unknown_channel`, evict the cached handle too before throwing (same eviction used on the first failure — cache-reference clear only; do NOT touch liveRoutes so late replies for other in-flight work on that handle still resolve). Keep the one-retry-per-call limit exactly as is.

Test (clients/subc-client/tests/): a managed call sequence where the daemon answers `unknown_channel` for both the original and the retried request must leave the route cache EMPTY afterwards (next call opens a fresh route rather than reusing the dead handle). Make it contrastive: assert today's buggy shape is gone by asserting the third call's first frame is a route.open, not a data request on the dead channel. Follow the existing test harness patterns in that directory (fake daemon socket helpers).

## 2. wire_version tripwire in the connection-file readers (TS + Swift)

The daemon is gaining an optional `wire_version` field (u8) in subc-connection.json (schema stays 1; a parallel Rust task adds the writer). Add reader-side enforcement so a future wire-version flip fails loud at discovery instead of at TCP:

- TS: clients/subc-client/src/connection-file.ts — parse optional `wire_version`; if present and !== PROTOCOL_VERSION (from envelope.ts), throw ConnectionFileError with a message naming both versions and saying the client library must be upgraded ("connection file wire_version X but this client speaks Y"). Absent field = accepted (older daemon).
- Swift: clients/subc-client-swift/Sources/SubcClient/ConnectionFile.swift — same rule against the Swift PROTOCOL_VERSION constant (Envelope.swift), throwing the module's existing error type with a descriptive message.

Tests both sides: file without the field parses fine; file with matching wire_version parses fine; mismatched wire_version fails loud with both versions in the message. Swift tests run under XCTest (swift test); TS under the existing runner.

## Verification bar

TS: full suite in clients/subc-client (npm test) + typecheck green. Swift: swift test green in clients/subc-client-swift. No version bumps, no publish — the release train is batched separately. Two commits: one per concern.
