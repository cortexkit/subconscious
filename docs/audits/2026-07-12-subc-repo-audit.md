# subc repo audit — 2026-07-12 (4 blind Oracles, gpt-5.6-sol-pro xhigh)

Four parallel blind Oracle audits, non-overlapping areas, each seeded with the
shipped by-design invariants and required to cite file:line. Totals across all
four: **5 BLOCKER, 11 HIGH, 10 MEDIUM, 1 LOW.**

STATUS: findings recorded. NOT yet source-verified by me — Oracle findings are
claims-to-verify (they cite file:line, so high-confidence, but every BLOCKER
gets an independent source check before any fix lands). No code changed.

Oracle tasks: core bg_16bc1233 · transport bg_d61d3ae2 · supervision
bg_ead3c036 · clients/MCP bg_274da070.

---

## CROSS-CUTTING THEMES (the important read — several findings share ONE root)

### THEME A — Unbounded `FrameSink.send()` on critical paths not covered by any deadline (SYSTEMIC, spans 3 audits)
The bounded 64-frame mpsc `FrameSink.send()` (router.rs:25-46) blocks waiting
for capacity when a module's egress queue is full. Multiple critical paths await
it WITHOUT the operation's deadline covering the enqueue:
- **Supervision BLOCKER**: `probe_module_health` awaits `module_sink.send` OUTSIDE
  `timeout_at` (supervise.rs:1366-1396) → a full module egress freezes the
  supervision actor: it stops polling `Child::wait` and supervisor commands →
  restart/reload cannot recover the module in-band.
- **Core HIGH (liveness)**: a credit-blocked `ChannelFlow::acquire` in the route
  future means `connection_loop` stops polling `read_frame` (server.rs:351-356)
  → later CANCEL/GOODBYE/EOF never observed → credit + route stay live forever.
- **Core MEDIUM**: route.bind timeout starts only AFTER `module_sink.send`
  completes (control.rs:1112-1121) → blocked open holds its RAII reservation +
  channel indefinitely; repeatable to channel-space exhaustion.
ONE STRUCTURAL FIX covers the class: every FrameSink.send on a critical/
deadline-bearing path must be bounded by that deadline (timeout_at over the
send) OR be a try_send that fails the op, and socket-read must never be starved
by a blocked dispatch. This is the highest-leverage fix in the audit.

### THEME B — Channel/identity reuse → cross-tenant frame misdelivery (core cluster)
- **Core BLOCKER**: after u16 channel cursor wrap, `allocate_module_channel`
  reuses a released module-local channel N in the same generation
  (forwarding.rs:974-1003, :992-994 self-documents the hazard) → a delayed frame
  for client A on N misdelivers to client B, and if terminal, corrupts B's
  credit (router.rs:264-302). Cross-tenant body exposure.
- **Core HIGH**: a second HELLO on the same socket swaps generation while
  reusing the connection + resetting the channel cursor (control.rs:326-335,
  forwarding.rs:279-306) → stale old-module frames misdeliver to new bindings;
  generation isn't on the wire so they're indistinguishable.
- **Core HIGH**: C1-cleanup vs C2-reconnect race unconditionally removes M from
  `modules_by_id` even when it now points at C2's fresh endpoint
  (forwarding.rs:1128-1131) → C2 gets HELLO_ACK but is permanently unroutable.
Common root: identity/generation transitions aren't coherent against
concurrent teardown, and channel IDs are reused within a generation.

### THEME C — Reconnect redrop wedge (identical bug, both client SDKs)
- **Clients BLOCKER (TS provider)**: `replaceConnection` installs the new socket
  while the prior reconnect promise still holds `this.reconnecting`; an immediate
  redrop no-ops `scheduleReconnectAfterDrop`, old promise clears the flag without
  rescheduling → provider disconnected until process restart; flapping daemon
  wedges providers fleet-wide (provider.ts:764-814).
- **Clients MEDIUM (Rust)**: same shape (consumer.rs:868-916) — new reader drops
  before `reconnecting` cleared → `spawn_reconnect` no-ops → restoration stalls
  until the next call. Less severe (a call re-primes it) but same class.
Fix: a pending-redrop latch re-checked after the new generation installs, both SDKs.

### THEME D — Spawn-attestation / reserved-identity gating gaps (transport)
- **Transport BLOCKER**: reserved-HELLO gate is FAIL-OPEN before config is
  installed (listener accepts at bootstrap.rs:355 before identities land at
  :367-371) AND for disabled reserved modules (no spawn nonce ever recorded) →
  a reserved security-boundary ID can be squatted with no launch nonce, and the
  squatter receives the claimed ID's storage descriptor + capabilities.
- **Transport HIGH**: launch attestations (spawn_nonces/reserved_nonces) are NOT
  cleared on disable/clean-exit/terminal-crash (only on retire) and liveness
  isn't consulted → a leaked nonce grants Reserved trust after the child is gone.
- **Transport HIGH**: non-reserved supervised IDs are universally squat-able
  (HELLO gate checks the nonce only for reserved IDs) → identity confusion +
  descriptor disclosure (does NOT grant Reserved — absent identity → Direct —
  but violates module-identity isolation).

### THEME E — `module_warming` missing on BOTH sides
Supervision has no warming ModuleState (marks Running at OS-spawn, pre-readiness,
supervise.rs:2339-2350) and the wire has no warming distinction (forwarding
exposes generic target_unavailable); clients' route-retry classifiers OMIT
`module_warming` (client.ts:809-825, consumer.rs:1102-1116) → a warming
rejection bypasses the 30s budget (TS terminal-fails, Rust returns NotSent).
One coherent fix: add the typed warming state daemon-side AND to both retry sets.

### THEME F — at-most-once sent-boundary drawn before the actual write (Rust)
Rust `writer_loop` sets accepted=true BEFORE `write_one_and_flush`
(consumer.rs:1390-1399) → a zero-byte-transferred write failure settles
OutcomeUnknown instead of NotSent (conservative — no unsafe dup — but suppresses
provably-safe retries). TS draws it correctly at socket.write handoff.

---

## BY AUDIT (full detail)

### Core routing/forwarding/concurrency — bg_16bc1233 (1 BLOCKER, 3 HIGH, 2 MEDIUM)
- BLOCKER: channel reuse cross-tenant misdelivery (forwarding.rs:974-1003) — THEME B
- HIGH: credit-blocked acquire starves CANCEL/GOODBYE/EOF (server.rs:329-356) — THEME A
- HIGH: second-HELLO generation bypass (control.rs:326-335) — THEME B
- HIGH: registry/forwarding teardown erases reconnected module (forwarding.rs:1123-1132) — THEME B
- MEDIUM: route.bind timeout misses relay enqueue (control.rs:1112-1121) — THEME A
- MEDIUM: cancel-after-relay-enqueue skips abandoned-bind GOODBYE (control.rs:178-202)
- CLEAN: no lock-across-await, RAII early-return release, panic isolation, other credit paths.

### Transport/auth/trust — bg_d61d3ae2 (1 BLOCKER, 2 HIGH, 1 MEDIUM, 1 LOW)
- BLOCKER: reserved-HELLO fail-open pre-config + for disabled modules (bootstrap.rs:329-371, supervise.rs:426-444) — THEME D
- HIGH: attestations valid after process stops (supervise.rs:506-575) — THEME D
- HIGH: non-reserved supervised IDs squat-able (control.rs:613-621) — THEME D
- MEDIUM: Windows owner-only ACL assumes XDG_RUNTIME_DIR unset (connection_file.rs:239-251)
- LOW: daemon_id compared with `!=` not constant-time (auth.rs:280)
- CLEAN: HMAC handshake, deadline/caps/semaphore, loopback-only, Unix atomic 0600 publish, capability relay verbatim, no secret logging.

### Supervision/health/lifecycle — bg_ead3c036 (2 BLOCKER, 5 HIGH, 2 MEDIUM)
- BLOCKER: probe enqueue outside deadline freezes actor (supervise.rs:1366-1396) — THEME A
- BLOCKER: `on_degraded: restart` allowed → kills slow-but-moving module (daemon_config.rs:398-435) — violates Health-Path v2
- HIGH: supervision-lane death suppresses probing instead of restart (supervise.rs:1223-1237; subc-mcp/main.rs:1428-1445)
- HIGH: subc-mcp health neither mechanical nor insulated (always Ok, awaits data-path mutexes; main.rs:1488-1499) — violates Health-Path v2 lock-set clause
- HIGH: reported-unresponsive can't do thresholded restart (no typed state; session.rs:16-30)
- HIGH: spawn failures escape terminal/recovery state machine (supervise.rs:1639-1669, 2145-2229)
- HIGH: failed rescan retirement recreates closed command channel, no actor backstop (supervise.rs:1894-1921)
- MEDIUM: module_warming absent + marked Running too early (supervise.rs:2339-2350) — THEME E
- MEDIUM: FD scaling doesn't guarantee child headroom (Unix clamp/warn; Windows per-process only; bootstrap.rs:246-299)
- CLEAN: Child::wait arms preserve actor, health-restart increments budget, reload no-overlap, shared op lock, cached supervisor.health, watchdog nonfatal+excluded.

### Clients + MCP gateway — bg_274da070 (1 BLOCKER, 1 HIGH, 4 MEDIUM)
- BLOCKER: TS provider loses post-reconnect redrop (provider.ts:764-814) — THEME C
- HIGH: TS managed routes can't preserve consumer_capabilities (client.ts:118-125…) → reverse MCP (sampling/elicitation) denied; Rust is correct
- MEDIUM: Rust immediate-second-drop after reconnect (consumer.rs:868-916) — THEME C
- MEDIUM: Rust marks sent before actual write (consumer.rs:1390-1399) — THEME F
- MEDIUM: both retry classifiers omit module_warming — THEME E
- MEDIUM: TS subscriptions lack bounded backpressure + drop-cancel + callback isolation (client.ts:399-452); Rust correct
- CLEAN: AuthError transient (all 3), TS deadline arbitration, GOODBYE evict + unknown_channel reopen (both), MCP facade policy fully clean (narrowing-only, default-deny, zero-tool exclusion, search meta-tools, spawn-identity required — NO trust-widening path).

---

## SUGGESTED TRIAGE (my read, for Ufuk)
1. THEME A (FrameSink enqueue deadline) — highest leverage: one structural fix
   clears 1 BLOCKER + 1 HIGH + 1 MEDIUM across core+supervision. Do first.
2. THEME C reconnect redrop — TS provider BLOCKER is Short (1-4h) and a
   fleet-wedge class; ship the latch to both SDKs early.
3. `on_degraded: restart` BLOCKER — trivial (reject the config value) + it
   re-creates the exact load-spike false-kill Health-Path v2 exists to prevent.
4. THEME B channel/identity coherence — largest (3d+), the channel-reuse BLOCKER
   is real cross-tenant exposure; needs careful design (channel retirement +
   generation-on-wire question).
5. THEME D attestation gaps — Medium (1-2d); real trust-downgrade paths, same-host
   trust model softens impact but they violate stated isolation invariants.
6. THEME E module_warming — coherent daemon+client fix, Medium.
7. Remainder (Windows ACL, const-time daemon_id, at-most-once boundary, TS
   subscription backpressure, supervision spawn/rescan state machine) — batch.

All BLOCKERs source-verified before any fix. Fixes are subc-core changes =
mine to own; the client-SDK ones (THEME C, capabilities, warming, subscription)
touch @cortexkit/subc-client + subc-client-rs and go in a client release train.
