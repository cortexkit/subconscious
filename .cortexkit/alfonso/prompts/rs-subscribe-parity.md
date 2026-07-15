# Build: subscribe() streaming parity in subc-client-rs

Add a held-open subscription primitive to `SubcConsumer` in
`crates/subc-client-rs/src/consumer.rs`, mirroring the TS client's
`subscribe()` (clients/subc-client/src/client.ts — read it first, it is the
reference contract and its doc comments explain the design).

## Contract (mirror of TS)

- `SubcConsumer::subscribe(target, identity, body, opts) -> Subscription`:
  opens (or reuses) the managed route for (target, identity) exactly like
  `call()` does, sends ONE Request the provider holds open, and delivers each
  interim `StreamData` frame for that (channel, corr) to an event callback.
  The terminal is `StreamEnd` (clean close), `Error` frame (reject with the
  decoded ErrorBody), or route GOODBYE / connection drop (reject).
- `Subscription` exposes: a way to receive events (prefer a
  `tokio::sync::mpsc::Receiver<Vec<u8>>` handed back to the caller over a
  callback — more idiomatic in Rust and avoids reentrancy while holding
  internal locks), a `closed()` future resolving on the terminal
  (Ok on StreamEnd, Err(CallError) otherwise), and `unsubscribe()` which
  sends a best-effort Cancel frame for the held-open corr and settles closed()
  promptly.
- Events ride the held-open request's correlation id — never unsolicited
  frames. The existing reader currently ignores StreamData/Push
  (consumer.rs frame dispatch, `FrameType::StreamData | FrameType::Push => {}`)
  — route StreamData for a subscribed (generation, channel, corr) to its
  subscription channel instead; leave Push handling unchanged.
- Flow-control: the held-open request holds one route-semaphore permit until
  terminal, same as the TS client holds credit (this is what a Serial
  provider expects).
- Reuse the existing machinery: generation-scoped demux, close-beats-reopen
  route teardown (a closeRoute on the route must settle the subscription as
  rejected + release its permit), reconnect invalidation (a generation drop
  rejects the subscription; NO auto-resubscribe — the consumer decides, per
  the durable-replay model: subscribers resubscribe with their own cursor).
- Backpressure: bounded event channel; if the receiver stops draining and the
  channel fills, drop the subscription with a typed error rather than
  buffering unboundedly or blocking the reader loop (document why: the
  reader task must never await on a slow consumer).

## Non-goals

- No auto-resubscribe on reconnect (cursor semantics are the consumer's).
- No changes to unary call(), the wire, or subc-core.
- Push frames remain ignored.

## Tests

Unit tests in the consumer.rs tests mod where feasible plus an integration
test in tests/real_daemon.rs following the existing patterns there (real
daemon + stub provider): the fake-aft-stub already has streaming support —
check its StreamData capabilities (crates/subc-core/src/bin/fake-aft-stub.rs
in the sibling checkout is NOT available here; use the stub binary the
existing real_daemon tests build, and if it lacks a streaming tool, extend
the test provider harness in tests/ the way close-route tests spawn scripted
providers). Required coverage:
1. Events delivered in order, terminal StreamEnd resolves closed().
2. Error terminal rejects closed() with the module's ErrorBody.
3. unsubscribe() sends Cancel and settles promptly; provider-side sees Cancel.
4. Route GOODBYE mid-subscription rejects closed() and releases the permit
   (a follow-up unary call on the same target re-opens and succeeds).
5. Slow-consumer overflow drops the subscription with the typed error.
Non-vacuity: at least one test must fail if StreamData routing is removed
(e.g. assert event payload bytes, not just terminal).

## Gates

cargo test -p subc-client-rs green with env -u SUBC_MODULE_ID -u
SUBC_LAUNCH_NONCE; clippy -D warnings native + x86_64-pc-windows-gnu; fmt;
check_comments. Keep the public API surface documented (doc comments in the
crate's existing voice). Level-triggered test sync, no sleeps-as-sync.
