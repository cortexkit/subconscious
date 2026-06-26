# SubcProvider managed reconnect — design (pre-build)

Status: design, pre-Oracle-gate. The inbound twin of the consumer `call()` managed
reconnect (already shipped in `client.ts`). Owned by Alfonso@subc. The named gating
dependency for ALF's manager M0 (the plugin self-connects as an effect PROVIDER; a
dropped inbound provider connection mid-effect must survive transparently).

## The contract (locked with ALF, folded into the frozen effect-schema)

The effect SEMANTICS are entirely ALF's handler (the durable `effect_dedup` table,
the 4-class `execute_effect` return, single-flight/stale-process rules). The TRANSPORT
owes exactly three things, plus one optional:

- **(a)** fast inbound-seam restore after a drop (reconnect + re-HELLO-register).
- **(c)** connection-state events with a **single monotonic transport epoch** on
  `restored`. ALF's `provider_registry.connection_epoch` + `effect_intent.intent_epoch`
  are SOURCED from this epoch, never module-minted. ALF records
  `current_epoch = max(seen restored.epoch)`, stamps intents, and runs ONE reconcile
  sweep over `sent_pending WHERE intent_epoch < current_epoch` when the epoch bumps.
- **debounce** `restored` transport-side so a reconnect STORM (flap: down→up→down→up)
  surfaces ONE `restored` per genuine stable reconnect, not per flap — so ALF's sweep
  fires once per real reconnect. Spurious flaps are harmless on ALF's side regardless
  (the sweep redrives through the dedup table), so debounce is a COST optimization, not
  a correctness gate — but it's the right side for it (transport owns flap-coalescing).
- **(b) OPTIONAL** undeliverable-result signal: when a handler produced a result whose
  terminal frame couldn't be written because the socket dropped, surface it so ALF's
  dedup takes the cheap redrive-returns-stored path (STATE A) instead of a full
  host-state reconcile. ALF confirmed STATE A is closed WITHOUT it (redrive + dedup), so
  this is a latency optimization. **Decision: DEFER to a fast-follow** — build (a)+(c)+
  debounce first (the contract minimum), add (b) only if the cheap path proves worth the
  surface. Flagged for the Oracle to confirm deferral is sound.

The three inbound-drop STATES this must handle (from the schema):
- **A** done-undeliverable: handler ran, result computed, socket dropped before the
  terminal went out. ALF: redrive → dedup returns stored result. Transport: (a) restore +
  (optionally b) the signal.
- **B** executing-same-process: socket dropped while the handler is still running.
  ALF: single-flight (await incumbent). Transport: (a) restore + (c) so ALF knows the
  caller's channel is gone; the handler keeps running and records `done`.
- **C** crashed-stale-process: the provider process itself died. Transport can't signal
  (it's gone). ALF: dedup stale-process rule + reconcile. Transport owes nothing.

## Current state (what's being upgraded)

`SubcProvider` is single-shot: `connect()` does one auth + HELLO handshake; `readLoop()`
reads until the socket drops, then sets `closedErr` and stays dead (no reconnect). The
upgrade wraps this in a reconnect supervisor, mirroring the consumer `SubcClient`
machinery (generation-tracked readLoop, capped-backoff retry, single-flighted reconnect).

## Design

### Generation-scoped I/O (Oracle BLOCKER 1 — required)
The provider, unlike the consumer, has handlers that ACTIVELY SEND (responses, stream
events, errors). A handler that started on socket S1 and completes AFTER a reconnect to S2
must NOT write its stale response onto S2 (it would corrupt S2's channel/corr space — those
are reused per connection). So all handler I/O is GENERATION-SCOPED:
- `readLoop(sock, generation)` threads `{sock, generation}` through `dispatch` into every
  handler-send path.
- Every send goes through `sendOn(sock, generation, frame)` which verifies
  `this.sock === sock && this.generation === generation` before writing, else DROPS (the
  generation is stale — a no-op, which is exactly the STATE-A "result undeliverable" case).
- Inflight keys are generation-scoped: `${generation}:${channel}:${corr}`, and controller
  cleanup is token-checked so a stale handler cannot delete a NEW generation's controller.
- `ctx.emit` on a streaming handler also routes through `sendOn` → a post-reconnect emit
  from a pre-reconnect handler is a silent no-op (not a write to the new socket).

### Handler abort semantics on drop (Oracle BLOCKER 2 — the ALF STATE-B contract)
A socket DROP is NOT a route GOODBYE and must NOT abort a durable effect handler's critical
section. The distinction:
- `ctx.signal` means "THE REPLY CHANNEL FOR THIS REQUEST IS GONE — anything you emit/return
  will be discarded." It fires on explicit Cancel, route GOODBYE, AND socket drop.
- A STREAMING handler observes `signal` to STOP EMITTING and unwind (else it leaks, looping
  on its own source forever after the consumer is gone).
- A DURABLE-EFFECT handler (ALF's `execute_effect`) must NOT abort its critical section on
  `signal` — it completes the host I/O and records its own durable outcome (`done`); the
  undelivered result is recovered by ALF's redrive+dedup (STATE A). The transport firing
  `signal` is HARMLESS to such a handler as long as the handler does not check it mid-effect.
- So the contract to ALF: `execute_effect` IGNORES `ctx.signal` during the accepted-effect
  critical section (fsync `executing` → host I/O → fsync `done` runs to completion regardless
  of `signal`); `signal` is advisory "reply channel gone" only. This is the one cross-boundary
  point — CONFIRM with ALF that its handler does not abort on signal mid-effect.
- The transport does NOT separately "abort all handlers on drop" (the old wrong step). On a
  drop it just (1) stops the old readLoop, (2) makes old-generation sends no-ops, (3) fires
  `signal` (advisory), (4) reconnects. Handlers complete on their own; their sends no-op.

### Reconnect supervisor
On an UNEXPECTED socket drop (readLoop ends with an error, NOT a `close()`):
1. **Mark the old generation stale + fire the advisory `signal`** for in-flight requests (so
   streaming handlers unwind; durable-effect handlers complete and self-record per the
   contract above). Do NOT cancel durable handlers' work.
2. **Reconnect with capped exponential backoff** (reuse the consumer's `ReconnectBackoff`
   {baseMs, capMs, maxAttempts}, injectable `sleep` for tests): re-read the connection
   file (the daemon may have republished with a new port/key), re-auth, re-send HELLO
   (manifest + launch_nonce — a reserved module re-proves its nonce on every
   re-registration).
3. **`duplicate_module_id` on re-HELLO is RETRYABLE** (load-bearing): subc rejects a
   duplicate HELLO while the prior registration is STILL LIVE — and after an abrupt drop
   subc may not have detected the dead connection yet, so a fast re-HELLO races the
   daemon's teardown of the stale registration. Treat `duplicate_module_id` as a transient
   → back off + retry until subc tears down the stale registration and accepts. (This is
   exactly what ALF's host-bridge `connectWithReconnectRetry` does; the provider needs the
   same, in the library.) Distinguish from a PERMANENT HELLO rejection (e.g.
   invalid_manifest, reserved-nonce-mismatch) which is fatal — do NOT retry those.
4. **On successful re-registration:** bump the monotonic `connectionEpoch`, restart the
   readLoop on the new socket (generation-guarded so the old socket's trailing reads can't
   corrupt state — mirror `this.sock === sock && this.generation === generation`).

### Epoch semantics
- `connectionEpoch`: monotonic counter, **starts at 1 on the initial connect**, increments
  by 1 on each successful re-registration. Never resets.
- Surfaced TWO ways (ALF-confirmed `pm_d4f4ba03`): (1) the **`currentEpoch(): number`
  GETTER** — synchronous, monotonic, returns the latest COMPLETED re-registration's epoch
  (never pre-increments for an in-flight reconnect); ALF stamps `intent_epoch =
  currentEpoch()` at INSERT time under its dispatch lock. (2) the debounced `restored{epoch}`
  EVENT — drives ALF's sweep trigger. ALF chose the getter over "last-event-seen" for
  PRECISION (stamping from the getter makes intent_epoch == the actual dispatch epoch,
  minimizing spurious sweep churn under a flap storm — last-event-seen lags during the
  debounce window and over-sweeps). Both are SAFE (ALF verified both interleaving directions:
  over-stamp-low is harmless via idempotent dedup redrive; stranding can't happen because any
  genuine reconnect after dispatch emits `restored` at ≥ M+1 > intent_epoch).
- THE GUARANTEED PROPERTY (the getter's contract, what the Oracle verifies): every value
  `currentEpoch()` returns is eventually trailed by a `restored` at a strictly-greater epoch
  if a genuine reconnect follows — because (1) the getter advances only on a COMPLETED
  re-registration, (2) every completed re-registration eventually emits a debounced `restored`
  at the final post-coalesce epoch, (3) that epoch is ≥ every `currentEpoch()` returned since
  the prior `restored`. A superseded-then-coalesced epoch increment is still covered: the
  coalesced `restored` fires at the FINAL epoch > the superseded one, so ALF's `< final` sweep
  includes any intent stamped at the superseded epoch. No epoch increment is ever lost to
  coalescing in a way that strands an intent.
- The getter NEVER returns an epoch for an incomplete registration: called mid-reconnect
  (socket down, not yet re-registered) it returns the LAST COMPLETED epoch — so an intent
  stamped then carries the pre-drop epoch and is swept by the next `restored`. (Belt-and-
  suspenders: ALF likely can't dispatch mid-reconnect anyway — the route is effaced, the call
  hits not_sent/outcome_unknown.)

### Connection-state callback delivery (the (c) API + Oracle MAJOR — serialized, not best-effort)
Because `restored` drives ALF's DURABLE reconcile sweep, `onConnectionState` delivery is NOT
fire-and-forget:
- Events are delivered IN ORDER, serialized (an internal queue drains one at a time, awaiting
  each `void | Promise` before the next) — never overlapping/interleaved.
- A pending debounced `restored` captures `{epoch, generation}` and re-verifies it is still
  current before firing (a drop during the debounce window cancels it).
- A `restored` callback that REJECTS/throws is NOT swallowed: the epoch is NOT marked
  delivered, and it is retried (re-enqueued) — because silently dropping a `restored` could
  strand ALF's `sent_pending` work until some unrelated future reconnect. (Other states —
  `down`/`reconnecting`/`connected` — log-and-continue on throw; only `restored` is
  reconcile-load-bearing and must not be lost.)

Add to `SubcProviderConnectOptions`:
```
onConnectionState?(event: ProviderConnectionState): void | Promise<void>;
```
where
```
type ProviderConnectionState =
  | { state: "connected";    epoch: number }   // initial establish (epoch 1)
  | { state: "down";         cause: Error }     // socket dropped, reconnecting begins
  | { state: "reconnecting"; attempt: number }  // each backoff attempt (observability)
  | { state: "restored";     epoch: number };   // re-registered + STABLE (debounced)
```
Matches the existing callback style (`onBind`, `onRouteGone`). `restored` is the
ALF-load-bearing one (carries the epoch their sweep keys off). `down`/`reconnecting` are
for ALF's handler to know the caller's channel is gone (STATE B) + observability.

### Debounce of `restored`
After a successful re-registration, do NOT fire `restored` immediately. Start a debounce
timer (configurable `restoredDebounceMs`, default ~250ms). If the connection stays up for
the window → fire ONE `restored{epoch:current}`. If it drops again DURING the window →
cancel the pending `restored`, continue reconnecting (the next stable reconnect fires one
`restored` with the THEN-current epoch). So N rapid flaps = one `restored`, carrying the
final epoch. (The epoch still increments per actual re-registration; only the EVENT is
coalesced — ALF sweeps `< current` so the coalesced single sweep covers all the skipped
intermediate epochs by construction.)

### close() vs drop
`close()` (intentional) must NOT trigger reconnect: set `closeStarted` first (as today),
so the readLoop's terminal error is recognized as intentional and the supervisor exits.

### Initial epoch (Oracle-flagged, RESOLVED by the getter)
The Oracle flagged that an event-only epoch source risks ALF reading epoch 0 if `connect()`
resolves before an async `connected` callback fires. The `currentEpoch()` GETTER resolves
this by construction: the epoch is set to 1 during handshake completion, BEFORE `connect()`
resolves, so `currentEpoch()` returns 1 synchronously from the first moment ALF holds the
provider. ALF never reads epoch 0. (This is an additional reason the getter beats event-only.)

## Oracle verdict (bg_1f6bb8ac) — NO-GO-as-written → fixes folded, now GO-WITH-CHANGES
The epoch/debounce model (the worry) was confirmed SAFE, not the blocker. Two BLOCKERs +
two MAJORs, all folded above:
1. ✅ BLOCKER 1 (generation-scoped I/O) — folded: all handler sends via `sendOn(sock,
   generation, …)`, generation-scoped inflight keys, token-checked cleanup.
2. ✅ BLOCKER 2 (abort-vs-STATE-B) — folded: drop fires advisory `signal` only; durable
   handlers complete + self-record; transport never cancels a durable critical section.
   Cross-boundary: CONFIRM ALF's `execute_effect` ignores `signal` mid-effect.
3. ✅ MAJOR (restored delivery serialized + not-best-effort) — folded: ordered queue, a
   rejected `restored` is retried not swallowed.
4. ✅ MAJOR (document stale-stamp) — folded in Epoch semantics.
RESOLVED open decisions: (b) deferred to fast-follow (Oracle: sound, STATE A closed by
dedup); provider retries effectively-unbounded with capped backoff (Oracle: right default,
a give-up provider is dead); `duplicate_module_id` retryable ONLY on RE-HELLO after a prior
successful registration — FATAL on the initial connect (Oracle MINOR: an initial dup is a
real config conflict). Build (a)+(c)+debounce + generation-scoping; (b) is the fast-follow.
