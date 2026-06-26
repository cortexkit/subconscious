# subc Rust consumer with managed reconnect

Status: DESIGN NOTE (pre-Oracle, pre-ALF-API-confirm). Author: subconscious (Alfonso).

## Problem

Every Rust process that *consumes* another subc module today hand-rolls a bare
`TcpStream`: connect, authenticate, `route.open`, send a request on the route
channel, read the response. None of them survive the daemon dropping the
connection (a restart, a `supervisor.reload`, a transient blip). When the socket
dies mid-flight, the in-flight call is lost and the cached route channel is stale.

Two concrete consumers need this now:

- **ALF's `HostConsumer`** (`alfonso-core-module/src/host_consumer.rs`): the
  module half of the host-bridge, calling back into the OpenCode plugin to
  dispatch effects. Bare `TcpStream`, single route, zero reconnect — the live
  module-side dispatch loop cannot run against it until it can survive a blip.
- **llm-runner's `SubcToolPlane`** (`llmr-subc/src/subc_plane.rs`): dispatches
  tool calls through subc to AFT. It already carries the proven
  `NotSent` vs `SentButOutcomeUnknown` classification (the at-most-once boundary),
  but is likewise a bare connection with no reconnect.

This is the same duplication class the serve side had before `subc-module`. The
TypeScript side already solved BOTH halves in one package
(`@cortexkit/subc-client`: `SubcProvider` serve + `SubcClient` consume, the latter
with a managed `call()` that shipped at 5e5c7327). The Rust side has the serve
half (`subc-module`) but no consume half. This note designs it.

## What already exists to lift

1. **The reconnect machinery** — proven on the *provider* side in
   `@cortexkit/subc-client` (`SubcProvider`) and verified end-to-end against a
   real daemon restart (exactly-once, generation-scoped sends, debounced
   `restored{epoch}`). The Rust consumer reuses the SAME pattern: an internal
   reconnect supervisor, a monotonic connection epoch (1 on first connect, +1 per
   successful reconnect+re-auth), and generation-scoped I/O so a stale socket's
   write no-ops.

2. **The at-most-once classification** — proven in llm-runner's `SubcToolPlane`:
   `NotSent` (the bytes never left the process — safe to re-emit) vs
   `SentButOutcomeUnknown` (the bytes went out but no terminal came back —
   re-emitting risks a double side-effect, so fail-to-doctor / idempotent
   re-dispatch on a stable id). This is the boundary that makes reconnect SAFE
   for mutating calls. The crate lifts this verbatim; it is not re-invented.

3. **The managed `call()` API** — proven in the TS `SubcClient.call()` Layer 1
   (cached route re-open, the `queued` boundary set synchronously when the socket
   accepts bytes, failure funneled through a single classifier). The Rust API
   mirrors it shape-for-shape so the two language clients stay symmetric.

## Crate structure (CONFIRMED with ALF — one crate, both roles)

`subc-module` is `publish = true` but NOT yet published to crates.io, so the name
is still free to change at zero cost.

The TS package hosts both roles in ONE package
(`@cortexkit/subc-client` = serve + consume). The symmetric Rust move is ONE
crate hosting both:

- `SubcModule` / `serve(...)` — the serve role (built, merged 855bde9a).
- `SubcConsumer` — the consume role with managed reconnect (this note).

DECISION (confirmed with ALF): rename `subc-module` → **`subc-client-rs`** (or
`subc-client`, modulo crates.io name availability) BEFORE first publish, and add a
`consumer` module alongside the existing serve code. One crate, two roles
(`SubcModule` serve + `SubcConsumer` consume), mirroring `@cortexkit/subc-client`.
Do the rename as the first build step so the published name is right.

Rejected alternative: a separate `subc-consumer` crate. It would duplicate the
connect+auth+frame-codec plumbing that the serve side already has, and split a
naturally-paired client surface across two crates for no benefit. The serve and
consume roles share the transport core; keep them together.

## API shape (mirror TS `SubcClient.call()` Layer 1)

```rust
pub struct SubcConsumer { /* owns connect+auth+reconnect supervisor */ }

impl SubcConsumer {
    /// Connect, authenticate, start the reconnect supervisor. Epoch starts at 1.
    pub async fn connect(connection_file: &Path, opts: ConsumerOptions)
        -> Result<Self, ConsumerError>;

    /// Managed unary call. Opens (or reuses a cached) route to `target`,
    /// sends `body` on the route channel, awaits the terminal frame.
    /// Transparently re-opens the route after a reconnect.
    pub async fn call(
        &self,
        target: RouteTarget,
        identity: BindIdentity,
        body: Vec<u8>,
        opts: CallOptions,
    ) -> Result<Vec<u8>, CallError>;

    /// Current transport epoch (1, then +1 per successful reconnect+re-auth).
    pub fn current_epoch(&self) -> u64;

    /// Reconnect lifecycle signal, symmetric with the provider's `restored`.
    pub fn on_connection_state(&self, cb: impl Fn(ConnectionState) + Send + 'static);
}

pub enum CallError {
    /// Bytes NEVER left the process (route-open failed, socket rejected the
    /// write before accept, etc.). Safe to re-emit — no side effect happened.
    NotSent(Box<dyn Error + Send + Sync>),
    /// Bytes WENT OUT but no terminal came back (socket died after accept).
    /// Re-emitting risks a double side-effect: fail-to-doctor, or idempotent
    /// re-dispatch keyed on a stable id the caller controls.
    OutcomeUnknown(Box<dyn Error + Send + Sync>),
    /// The module's HANDLER threw (unknown op / malformed request) → a typed
    /// Error frame on the wire. This is a real, DELIVERED outcome — a dispatch
    /// bug, NOT a transport failure and NOT an application-level effect rejection.
    /// (An effect REJECTION rides a SUCCESSFUL call() inside the response body —
    /// e.g. `{result:{outcome:"terminal", errorCode,...}, epoch}` — which the
    /// caller parses from the returned bytes; it is NOT a CallError. Only a
    /// handler-threw maps to CallError::Module. Confirmed with ALF.)
    Module(ErrorBody),
}

pub enum ConnectionState { Dropped, Restored { epoch: u64 } }
```

Load-bearing properties (all mirrored from the proven sides; the per-target +
concurrent + transient-absence points are ALF's confirmed `HostConsumer`
requirements):

- **The `NotSent`/`OutcomeUnknown` cut is the whole point.** It is set by where in
  the send path the failure occurred, exactly as `SubcToolPlane` does it today —
  the `queued`/accepted boundary is the dividing line. `NotSent` ⇒ the managed
  layer MAY transparently retry across a reconnect (no observable effect yet).
  `OutcomeUnknown` ⇒ it MUST NOT auto-retry; it surfaces to the caller, who owns
  the idempotency key / doctor decision. This matches llm-runner's
  interrupt/fail-to-doctor default for non-idempotent INDETERMINATE outcomes.

- **Per-target route map (NOT a single route).** `HostConsumer` talks to N
  providers at once (each plugin instance is a distinct `alfonso-host:<provider_id>`
  module_id, from GATE 1). So the consumer caches and maintains a route PER target
  module_id, keyed by the full `(target, identity)`. `call(target, …)` looks up or
  opens that target's route; there is no "connect to one route" mode. On
  `restored`, ALL live target routes are INVALIDATED but re-opened **lazily on next
  use** (LOCKED — ALF voted lazy explicitly). Eager re-open of N routes on every
  blip is a thundering re-open of routes that may never be called again, and buys
  nothing: ALF keys its reconnect-sweep on the PROVIDER epoch in the
  `{result, epoch}` response body, so it only learns of a provider reconnect by
  making a `call()` — and the `call()` IS the thing that re-opens the route and
  observes the fresh epoch. Lazy composes correctly with the reconcile loop; eager
  is wasted work. (ALF owns a periodic `host.ping` idle-floor so epoch observation
  doesn't starve when no effects are pending — its side, not the crate's.)

- **Concurrent in-flight, multiplexed by `corr` over the one connection.** The
  dispatch loop fans out multiple `execute_effect` calls concurrently (same or
  different targets). `call()` must be safe under concurrency with multiple
  outstanding `corr`s — a shared connection with a corr→oneshot demux map (the
  same demux-by-(channel,corr) the TS client and cortexkit-e2e use), NOT
  serial-per-call. This is a step up from `HostConsumer`'s current single-route
  serial model.

- **Route re-open MUST tolerate a transiently-absent target = retryable
  `NotSent`.** THE cross-restart correctness case (and exactly what TEST 2
  exercises): after a daemon restart BOTH connections drop; the consumer
  reconnects and re-`route.open`s to `alfonso-host:<pid>`, but the plugin provider
  may not have re-HELLO'd yet. That MUST be a RETRYABLE `NotSent`-class condition
  (backoff + retry until the provider re-registers), NEVER a hard terminal — the
  call provably never reached a handler, so re-emit is safe.

  EXACT subc error codes to classify (verified at source, control.rs
  `handle_route_open` 715-788; the consumer must key on the CODE, not message
  text):
  - `unknown_module` — target is in NEITHER the registry NOR the supervisor
    snapshot. **THIS is ALF's steady-state transient case**: the plugin provider
    is self-connecting (not daemon-supervised), so after a restart, before it
    re-HELLOs, it falls through to `unknown_module` (NOT `target_unavailable`,
    which only fires for a *supervised* module that is inactive). RETRYABLE.
  - `module_reloading` — a `supervisor.reload` drain is in progress. RETRYABLE.
  - `target_unavailable` — overloaded: transient (supervised-not-available / not
    active / not live / no live forwarding connection) BUT ALSO permanent (the
    module exists but does not provide the requested role — a misconfiguration).
    Treat as RETRYABLE-bounded: the permanent role-mismatch case is a caller bug
    that SHOULD surface, and the cap+deadline guarantees it terminates as a
    `NotSent` error rather than spinning forever.

  The retry MUST be bounded (cap + deadline) so a permanently-gone or
  misconfigured target eventually surfaces a terminal `NotSent` to the caller, not
  an infinite spin. `module_timeout` (the bind relay was enqueued but the module
  did not answer) is also pre-data-send NotSent-class and route-open-retryable.
  Precedent: subc already proved a bind-retry helper for the symmetric
  cold-AFT-configure race (cortexkit-e2e).

- **Cached route re-open on epoch change.** Cache `(target, identity) ->
  {channel, epoch}`; on epoch mismatch (a reconnect happened), re-open before
  sending. Same as TS `CachedRoute.generation`.

- **Generation-scoped I/O.** A response/terminal read on the OLD socket is
  discarded — a call's result is only valid on the socket generation it was sent
  on. Same guard as the provider side.

- **Capped exponential backoff** on reconnect AND on the transient-absent-target
  retry (the provider side used 100ms→2s, 6 attempts; match it).

- **Consumer epoch is observability-only, NOT load-bearing for caller
  correctness.** ALF keys its reconnect-sweep on the PROVIDER epoch, which rides
  on the host RESPONSE envelope (`{result, epoch}`) and is read from `call()`
  response bodies — NOT from the consumer transport. So `current_epoch()` /
  `on_connection_state(restored{epoch})` exist for internal cached-route-reopen
  triggering + logging, but the crate does NOT need to surface a consumer-side
  epoch as a correctness signal to the caller. Keep it minimal (don't over-build a
  consumer-epoch contract).

## Resolved scope (was open questions; ALF confirmed)

1. **Unary only.** All host ops (`ping`, `session_status`, `execute_effect`) are
   unary request→response. No streaming/`subscribe` on the consumer side. Ship
   unary-only; a `subscribe()` can be added later if a future consumer needs it
   (TS `SubcClient` has both, but no current Rust consumer does).
2. **Many concurrent targets + concurrent in-flight** — folded into the
   load-bearing properties above (per-target route map + corr-multiplexed
   concurrency).
3. **The one `HostConsumer`-specific correctness need** — the transiently-absent
   target on re-open = retryable `NotSent` — folded in above as a first-class
   requirement and the focus of the proof test.

## Implementation contract (Oracle-gated — must-fix before build)

Oracle verdict: GO-WITH-CHANGES. Five blockers, all folded here as binding
implementation requirements. (The Oracle also caught a factual error in my
routing-code summary — see the `unknown_module` correction in the transient-absent
section above; it was the wrong code for ALF's self-connecting provider and is now
fixed.)

1. **Define the "sent" boundary CONSERVATIVELY.** Rust `write_frame` uses
   `write_all`, which cannot prove "zero bytes left" after a partial-write error.
   So: `NotSent` ONLY before the frame is handed to the writer path at all; once a
   writer task accepts it or the first write byte is attempted, ANY failure is
   `OutcomeUnknown`. Never infer `NotSent` from a mid-write error. (This is
   stricter than the TS `queued` boundary because `write_all` partial-writes are
   ambiguous — err on the side of OutcomeUnknown, the safe-against-double-effect
   classification.)

2. **Pending demux MUST be generation-scoped.** Keying pending oneshots by
   `(channel, corr)` alone races: gen-1 sends `(ch7, corr1)`, reconnect opens
   gen-2, the channel/corr get reused, and a late gen-1 response resolves gen-2's
   waiter. Key by `(generation, channel, corr)` (or a per-generation pending map).
   On socket drop, SETTLE every pending waiter of that generation: pre-accept ⇒
   `NotSent`, post-accept ⇒ `OutcomeUnknown`. Never leak a waiter across a
   reconnect.

3. **Per-target `route.open` single-flight.** Two concurrent same-target `call()`s
   that both miss the cache must NOT both `route.open` (provider runs bind/config
   twice, one route later orphaned). Use a per-key `opening` future/slot (the TS
   `CachedRoute.opening` shape). Do NOT hold a mutex across the `.await`.

4. **Target-absence retry: code-specific, bounded, lazy-per-call.** Retry ONLY the
   transient codes (`unknown_module`, `module_reloading`, and the transient
   `target_unavailable`/`module_timeout` cases) — see the verified taxonomy above.
   Bound it (cap + deadline) → a permanently-absent/misconfigured target surfaces a
   terminal `NotSent`, never an infinite spin. The retry is INTERNAL to the
   consumer (the caller sees only the exhausted terminal `NotSent`, not per-attempt
   — ALF confirmed this split: consumer owns the short re-HELLO gap, ALF's
   reconcile driver owns the longer `OutcomeUnknown` horizon, no double-retry). The
   retry loop must NOT hold a lock across `.await` and must NOT starve other targets'
   calls. And it must NEVER retry an `OutcomeUnknown` (only pre-send route-open
   failures are retryable).

5. **Drop / `close()` cleanup contract (not just reconnect creation).** Dropping
   `SubcConsumer` (or an explicit `close()`) must: cancel the reconnect
   supervisor + backoff sleeps, close the active socket, stop reader/writer tasks,
   clear route + opening state, and SETTLE all pending callers (no task/waiter
   leak). Use RAII pending-cleanup (llm-runner's `PendingRegistration` shape) so a
   CANCELLED `call()` future (dropped after registering its waiter, before/after
   send) does not leak the waiter.

Non-blocking (fold as judgment): consider a local per-route flow-control semaphore
— subc's `ChannelFlow::acquire().await` blocks reads of LATER frames when a route
window is exhausted, so a saturated serial route can head-of-line-block unrelated
target calls on the same TCP connection; a client-side per-route concurrency bound
(matching the provider's declared concurrency) avoids feeding subc past its window.
Consumer epoch stays observability-only EXTERNALLY, but generation/epoch remains
load-bearing INTERNALLY (route-cache invalidation, pending demux key, stale-I/O
discard) — do not let "observability-only" leak into dropping the internal
generation guard.

## Process

This is concurrency-critical AND at-most-once (same risk class as the provider
reconnect, which was Oracle-gated and the Oracle caught 2 real blockers —
generation-scoped handler I/O and an abort-vs-durable-execution contradiction).
Status: API confirmed by ALF; Oracle-gated (GO-WITH-CHANGES, 5 blockers folded
above). Remaining:

1. ~~ALF confirms the API shape + the 3 open questions.~~ DONE.
2. ~~Oracle-gate THIS note.~~ DONE (blockers folded into the contract above).
3. Build, lifting the reconnect machinery + the `NotSent`/`OutcomeUnknown`
   classification, NOT re-inventing either, satisfying all 5 contract items.
4. Prove with real-daemon reconnect tests (mirror the provider-side live test's
   rigor):
   - kill the daemon mid-`call()`; assert `NotSent` is safely re-emitted and
     `OutcomeUnknown` is NOT auto-retried, and a subsequent `call()` transparently
     re-opens its route.
   - **the TEST-2 race**: restart the daemon so BOTH connections drop, then issue
     a `call()` BEFORE the target provider has re-HELLO'd — assert `route.open`'s
     `target_unavailable` is classified retryable-`NotSent`, backs off, and
     succeeds once the provider re-registers (never a hard terminal). Also assert
     the bounded case: a target that never returns eventually surfaces a terminal
     `NotSent` rather than spinning.
   - **concurrent multiplex**: N concurrent `call()`s to ≥2 distinct target
     module_ids over one connection complete correctly (corr demux, no
     cross-talk), and survive a mid-flight reconnect with the right
     per-call classification.
5. Migrate `HostConsumer` AND `SubcToolPlane` onto it (both delete their
   hand-rolled connection + gain reconnect for free). This is the
   dedup-the-class win, the consumer twin of `subc-module`.

## Non-goals

- Not a higher-level RPC / typed-method layer — `call()` is opaque bytes in,
  opaque bytes out, exactly like the wire. Typed surfaces are the consumer's.
- Not durability / exactly-once GUARANTEES — subc is a thin router that silently
  drops frames to dead channels. The crate provides the at-most-once
  CLASSIFICATION (so the caller can make the right re-dispatch decision); durable
  dedup/outbox stays in the consuming module (the same split llm-runner and ALF
  already own).
