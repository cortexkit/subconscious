# subc-client-rs: race flow-control acquire against the call deadline (F4) + reconnect stale-promise audit

Two items in crates/subc-client-rs (subconscious repo). Do not touch clients/ (TS/Swift are separate tasks).

## 1. F4: `SubcConsumer::call` can park past its own deadline (consumer.rs)

In `SubcConsumer::call` (consumer.rs, around line 345 — verify at HEAD), `route.sem.acquire_owned().await` (the per-route flow-control window) is awaited BEFORE the call deadline is consulted; the deadline is only applied afterwards via `remaining_duration`. A caller with `CallOptions.timeout = 10s` on a route whose 32-credit window is saturated by slow requests parks on the semaphore indefinitely — the timeout is not honored by construction. (A downstream consumer, thalamus, papered over this with an external `tokio::time::timeout` wrapper; the SDK should be correct by construction.)

Fix: race the semaphore acquire against the call deadline (e.g. `tokio::time::timeout_at(deadline, route.sem.acquire_owned())`), classifying a deadline expiry during acquire as NOT_SENT (the request never reached the wire — this classification matters: not_sent is safely retryable, outcome_unknown is not). Audit `subscribe()` and any other path that acquires route credit or other capacity before writing (route-open single-flight wait, control RPC paths) for the same shape and fix uniformly.

Tests: a route whose window is fully held by a hung in-flight request; a second call with a short timeout must fail at the deadline with the not_sent classification (this must FAIL against current code by hanging — use a test timeout to make the failure crisp), and must NOT have written any frame for the second request (assert on the fake daemon's received frames). Existing test patterns live in crates/subc-client-rs/tests/real_daemon.rs (real-daemon harness) and src unit tests; prefer the lighter harness that can hold credits deterministically.

## 2. Audit: stale-reconnect-promise wedge class in the Rust reconnect paths

The TS client had a production wedge: a never-settling reconnect promise made all later drops silent no-ops (`if reconnecting { return }`). Audit the Rust consumer's managed-reconnect path (consumer.rs) and the module-serve reconnect path (lib.rs, if any) for the same shape: any flag/handle meaning "a reconnect is in flight" that (a) can survive its task dying/hanging and (b) gates new reconnect attempts. If the shape exists, fix with generation-scoped supersession (a new drop for a newer generation replaces the in-flight attempt; stale attempt completion is a guarded no-op). If the shape structurally cannot occur (e.g. reconnect is driven by a single owning task with no early-return gate), document WHY in a code comment at the reconnect entry point and say so in your report — do not invent a fix for a non-bug.

## Verification bar

env -u SUBC_MODULE_ID -u SUBC_LAUNCH_NONCE cargo test -p subc-client-rs green (the env-unset matters: leaked module env vars are a known false-failure class); cargo clippy -p subc-client-rs --all-targets -- -D warnings clean natively AND --target x86_64-pc-windows-gnu; cargo fmt. One commit per item.
