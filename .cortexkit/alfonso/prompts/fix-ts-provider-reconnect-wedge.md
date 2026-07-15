# TS SubcProvider: fix the stale-reconnect-promise wedge (drop-driven re-entrancy + surfaced exhaustion)

One production-proven bug in clients/subc-client/src/provider.ts (subconscious repo). Do not touch other packages; the Rust side is a separate task.

## The bug (real prod incident)

A fleet of in-process `SubcProvider` instances survived one daemon restart, then a second restart 14 minutes later permanently wedged one instance: it never attempted another connection. Mechanism at source: `scheduleReconnectAfterDrop` early-returns when `this.reconnecting` is non-null. A reconnect promise from the earlier cycle that never settled (e.g. its socket died in a state the read loop never mapped to settled/failed) makes every LATER drop a silent no-op — the provider believes a reconnect is in flight forever. Silence is indistinguishable from a wedge for the caller: today the provider's indefinite-retry policy means no event, no rejection, nothing observable.

## Required fixes

1. DROP-DRIVEN RE-ENTRANCY: a new connection-drop must never be silently swallowed because an older reconnect promise exists. Generation-scope the reconnect state: when a drop arrives for the CURRENT socket generation and the recorded reconnect belongs to an older generation (or its socket is already dead), supersede it — start a fresh reconnect cycle and make the stale cycle's eventual settlement a no-op (generation guard on its completion path, matching the existing generation-scoping patterns in this file). Preserve single-flight for genuine duplicates (two drops of the same generation while a live reconnect for that generation runs = still one cycle).
2. SURFACED STATE: the provider already has a `ProviderConnectionState` event surface ({connected|down|reconnecting|restored}, provider.ts ~line 258). Ensure every reconnect cycle emits `reconnecting` with the attempt number, and that a superseded/stale cycle cannot emit events over the new cycle's (generation guard on emission). Add a `down` emission carrying the cause when a reconnect cycle is superseded after its socket died — so a watchdog reading events can always distinguish "retrying" from "dead silence". Do NOT change the indefinite-retry policy itself.
3. SELF-CONSISTENCY BACKSTOP: on completing a reconnect (HELLO_ACK accepted), assert the invariant that `this.reconnecting` is cleared before `restored` is emitted, so a settled cycle can never linger as a wedge.

## Tests (clients/subc-client/tests/provider.test.ts has the fake-daemon harness and a "managed reconnect" describe block to extend)

- CONTRASTIVE wedge repro: simulate the incident — first drop starts a reconnect whose socket the fake daemon accepts and then kills WITHOUT completing the handshake, leaving the first cycle pending; then a second drop arrives. Assert the provider starts a fresh cycle and successfully re-registers (this test must FAIL against the current code's early-return).
- Single-flight preserved: two rapid drops of the same generation produce exactly one HELLO re-registration.
- Event ordering: reconnecting(attempt) events observed for the superseding cycle; no events from the stale cycle after supersession.

## Verification bar

bun test (full suite in clients/subc-client) + npm run typecheck green. No version bump, no publish (a batched release train follows). One commit.
