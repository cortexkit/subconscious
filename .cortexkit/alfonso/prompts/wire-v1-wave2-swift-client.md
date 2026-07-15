# Wire v1-final — Wave 2: Swift client (clients/subc-client-swift)

You are implementing the Swift client's side of a frozen, Oracle-gated wire revision. Authoritative spec: `docs/specs/subc-wire-v1-final.md` (v7) — READ IT FULLY, especially §1 (layout), §2 (prefix-first), §3.2.1 (RouteHandle), §3.3 layer 2 (endpoint validation), §5 (admission class). Wave 1 landed the Rust wire shapes on master; daemon logic lands in a parallel wave — your tests are unit/parser level against fixtures and mocks, not a live daemon.

## Scope: clients/subc-client-swift ONLY.

1. **Envelope.swift**: 21-byte header — epoch UInt32 LE at [9..13], corr at [13..21], HEADER_LEN=21, PROTOCOL_VERSION=2. Admission-class bits4-5 (typed enum, default normal), reserved mask bits6-7. Decode rejections mirroring the Rust taxonomy (class 11, SHEDDABLE outside Push/StreamData, nonzero epoch on channel 0, unsupported version) as typed errors.
2. **Prefix-first reads**: the frame reader validates the 5-byte prefix (ver) before consuming the remaining header; preserves body-cap behavior.
3. **RouteHandle**: immutable struct { channel, epoch } + opaque connection token (socket-generation identity). All route-scoped operations in Client.swift (unary calls, subscribe/session streaming, cancel, close) take the handle; bare-channel paths removed. Token mismatch with the current connection → typed local error, no frame emitted. route.open consumes route_epoch and installs the handle into the client's channel→epoch map BEFORE returning to the caller and before processing further ingress.
4. **Endpoint ingress validation** (§3.3 layer 2): channel→epoch map; every nonzero-channel ingress frame validated before dispatch — mismatch/unknown → silent drop (counter). Applies to Request/Response/StreamData/StreamEnd/Error/Cancel/Goodbye alike; in-flight state keyed by (channel, epoch, corr). The existing mid-turn GOODBYE handling stays, but only fires for a GOODBYE matching a live handle.
5. **Late-RouteOpen cleanup**: a timed-out route.open that later receives a successful RouteOpen closes the returned handle with GOODBYE rather than dropping it.
6. **Corr hygiene**: channel-0 corrs from one monotonic counter, no reuse per connection.
7. **SubcChat/probe call sites**: update ChatViewModel/RoomsViewModel/ObserveViewModel + SubcSwiftProbe to the handle-based API — mechanical threading; behavior unchanged.
8. **Admission class**: optional per-call parameter (default normal) stamping bits4-5.

## Tests (Swift Testing / XCTest per existing suite)
Header encode/decode parity incl. epoch boundaries (0/1/UInt32.max) + admission classes + all rejection cases (exact typed errors); prefix-first stale-17-byte pure-header frame → prompt unsupported-version (bounded); stale-epoch ingress dropped without settling in-flight; token fencing (stale handle after reconnect emits no frame); wire-vector parity tests updated to the wave-3 shared golden shapes (keep fixture file format compatible with the existing cross-language vector files; final regen is coordinated in wave 3).

## Verification bar
`swift build` and `swift test` green on macOS. No bare-channel public route-scoped API remains. Report per-item status + test totals + API change list (the chat app call-site diff summarized).
