# llm-runner as a subc module — design (v4, build spec)

Status: v3 was reviewed by an Oracle verification pass (bg_7fe10e94, NO-GO-constructive)
that confirmed v3 closed 8/14 council findings but found the **new multi-turn lineage
model** had 4 blockers + 1 major against the real `llmr-core` (all inside
llmr-core/WAL/store — subc thin-core holds). v4 folds those fixes. This is the LLMRUNNER
build spec; the wire surface is owned by Alfonso@subc, the llmr-core/module work is
LLMRUNNER's.

> **v3 → v4 (the Oracle's blockers, fixed):** (B1) replay is now an explicit
> EPISODE STATE MACHINE — `RunStarted` opens an episode, prior episodes become prefix,
> only the last unfinished episode is resumable (v3 leaked one episode's terminal into
> the next). (B2) the replay cursor is a DURABLE `(wal_seq, sub_index)` derived from the
> enriched WAL — the in-process live seq is internal-only and never crosses a re-open
> (v3's process-local `event_seq` couldn't span projection↔live or survive coordinator
> re-open). (B3) the durable queue carries `submission_id` identity so consume/retract
> is unambiguous across a crash (v3's `SteerQueued` had only `message`). (B4)
> AuthRequired is a NON-terminal `RunPaused` record, not a `RunFinished` (replay treats
> `RunFinished` as terminal, so v3's "resumable terminal" would have sealed the run).
> (M5) the session coordinator is an explicit lineage ACTOR (Opening|Open|Closing|Closed)
> and WAL append rejects a stale lease epoch.
>
> **v2 → v3 (recap):** the WAL stores assembled durability units, not the live event
> stream — replay is honestly scoped + the WAL is enriched; the surface needs multi-turn
> follow-up runs the core lacked; the fork seam is now a real `origin` field; the queue
> is durable.

---

## 1. What it is

llm-runner becomes the **orchestration tier**: a subc module that runs agentic LLM
loops on behalf of consumer modules (dreamer, Alfonso's manager, the CK app), while
itself consuming AFT (tools), the credential vault (auth), and the router (model
selection). First module that is both a **provider** to consumers and a **major
consumer** of other modules. The agentic core is enhanced (not replaced) and shared with
the `llmr-run` CLI.

## 2. Two consumer roles

- **Drivers** start/steer work + inject prompts (dreamer, Alfonso, CK app).
- **Observers** read a session's content + watch it live (CK app's session view).

The CK app is **both**. The surface is **session-centric**: consumers address
*sessions*; a *run* is an internal episode.

## 3. Foundational model: a session is a LINEAGE OF RUNS (turns)

- **Session / lineage** — the durable conversation. One single-writer **lease**, one
  **WAL**, one **store** partition, keyed `{project_root, harness, session}`.
- **Run = one turn's loop execution** — a user message in → the agent loops until its
  response → a terminal. Each run begins with a `RunStarted` **episode marker** (input +
  frozen config + `origin` + `submission_id`). Runs **accumulate** in the lineage WAL:
  `RunStarted₁ …turn-1… RunFinished₁ RunStarted₂ …turn-2… …`. **One ACTIVE run per
  lineage at a time** (lease-enforced); idle between runs. **Chosen: one run per turn.**
- **Episode state machine (the B1 fix — replay is episode-aware):** replay walks the WAL
  as a sequence of episodes. Each `RunStarted` **closes/validates the previous episode,
  resets per-episode state (terminal flag, open tool batches, finish state), and appends
  the previous episode's messages to the lineage PREFIX.** Only the **last** episode is a
  resume/continue target; all prior episodes are immutable conversation prefix. A durable
  `RunFinished` for episode N must NOT mark the lineage finished — it marks episode N
  finished. There is an explicit **append-new-episode** path (idle `session.send` after a
  finished episode writes a fresh `RunStarted`), distinct from resume-the-last-unfinished.
- **Conversation history** = the prefix across all closed episodes ⊕ the current episode.
- **Submission** = one prompt injection. PENDING (durably queued, retractable) → consumed
  into a run (coordinator writes `RunStarted{submission_id}`) → ACTIVE → TERMINAL.
- **Session coordinator** (§12) — the in-process lineage owner that drains the durable
  queue and starts each run.
- **Fork** (future) — a separate lineage `SeededFrom` a parent; v4 lands the durable
  `origin` field (§18); the cross-lineage prefix read is deferred.

## 4. The wire surface

```
session.read   { session, after?: cursor, limit? }
    → { content, head: cursor, next?: cursor }                       (OBSERVER; §15)

session.subscribe { session, from?: "start" | "live" | cursor }      (dedicated route; §9)
    → stream of events (control reliable+replayable, display live+lossy); see §7,§8,§16

session.send   { session, prompt, model?, tool_providers?, stop_when?, seed_from? }
    → { submission_id, state: "active" | "pending", run_id? }        (DRIVER; §11)

session.retract { session, submission_id }
    → { retracted: true } | { retracted: false, reason: "already_started" }   (§11)

run.cancel  { run_id }   → { ack }          (cooperative → Interrupted; §13)
run.status  { run_id | session }
    → { state, run_id?, step?, last_error?, head: cursor }           (state enum §14)
```

A **cursor** is a durable `(wal_seq, sub_index)` (§7). `subscribe`/`read`/`status` share
this one cursor domain, so `read → subscribe(from: head)` is a lossless handoff.

## 5. Reliability model (the egress reality)

subc module→client egress is best-effort `try_send` into a 64-deep buffer; a full client
queue **CLOSES** that client (verified subc-core router.rs/server.rs). So **"reliable
control lane" = DURABLE + REPLAYABLE**, never guaranteed-live — a dropped consumer
**re-subscribes from its last cursor and replays**. **Display = live-only + lossy**
(coalesce/drop + `DisplayGap`; no per-token durability).

## 6. Multi-client fan-out + per-subscriber backpressure

subc is 1:1, no broadcast → **llm-runner owns the fan-out**. Per subscriber: **display
lossy** (coalesce/drop + `DisplayGap`); **control bounded-then-DETACH** the route (it
resubscribes from its cursor) — never blocks the run, never silently drops control. One
slow subscriber never affects the run or co-subscribers.

## 7. The control stream: durable cursor + in-process live tail (the B2/#1/#2/#13 fix)

The control stream is sourced from the **enriched WAL** (§18 adds the missing facts) via
a **deterministic projection** `WAL+store → Vec<ControlUnit>`, addressed by a **durable
cursor `(wal_seq, sub_index)`** — one record can project to several ordered ControlUnits,
disambiguated by `sub_index`. This cursor is **durable and lineage-scoped**: it is what a
consumer holds and **resubscribes with**, and it **survives the coordinator's
release/re-open** (§12) because it is derived from the WAL, not from process memory.

For the **not-yet-fsynced live tail** of the active run, the coordinator keeps a small
**in-process event log**: the loop appends each event to it **non-blocking** (NOT
`await out.send` per event — the #13 producer-stall fix), and the fan-out task drains it.
On fsync, those events acquire their durable `(wal_seq, sub_index)`. The **attach
barrier** (under the coordinator lock): capture the durable head cursor `H` and the live
tail; replay the projection `≤ H`, then stream the live tail, **deduplicating by the
durable cursor** for any tail event that crossed `H` during attach. **The in-process live
seq is internal only** — it is never handed to a client and never used as a resubscribe
cursor (so a re-open's reset cannot break a client). Display events are live-only (not in
the projection — a finished turn shows assembled text, not re-typed tokens).

## 8. Replay scope (honest)

`from:"start"` (or a cursor) = the durable projection (§7) from that point, then the live
tail. The projection is **semantically equivalent, not event-identical** to the original
live stream (display tokens are not reproduced). The §18 WAL enrichment is what makes it
faithful for step boundaries, finish reasons, tool names, and error detail.

## 9. Subscribe routing

`session.subscribe` is a **dedicated long-lived stream route**, never multiplexed with
`cancel`/`status`/`read`. `from:"live"` on a terminal run returns an immediate
synthesized terminal or `run_not_active` (§16).

## 10. Admission barrier + early failure

Idle `session.send` returns `{state:"active", run_id}` **only after** the run acquires
the lease and writes durable `RunStarted`. **Model/auth resolution happens BEFORE
`RunStarted`** so a resolution failure is a typed `session.send` error, not a ghost run.
A failure *after* `RunStarted` is a legitimate short terminal run answerable via
`run.status`, error persisted.

## 11. Submission lifecycle + durable queue + retract (the B3 fix)

- **Idle send** → starts a run (active, run_id).
- **Running send** → durably queued as **`Queued{submission_id, message}`** (a WAL
  record carrying IDENTITY, not just the message). State "pending".
- **Retract** → a durable **`Retracted{submission_id}`** tombstone, taken **atomically
  under the coordinator lock** (remove-if-pending vs `already_started`). Crash-safe.
- **Consume** → the coordinator writes **`RunStarted{submission_id, run_id, …}`**
  referencing the submission + emits `SubmissionStarted{submission_id, run_id}`.
- **Replay is unambiguous:** the pending set = `Queued − Retracted − consumed-by-a-RunStarted`.
  A crash BEFORE the consuming `RunStarted` ⇒ still pending (re-drained on re-open); a
  crash AFTER ⇒ that run is active/resumable, never double-consumed. FIFO; each queued
  prompt becomes its own episode.

## 12. Session coordinator = a lineage actor (the M5 fix)

One in-process **lineage actor** per open session with states **`Opening | Open |
Closing | Closed`**. ALL `session.send`/`retract`/`subscribe` for a lineage go through
its actor; a teardown cannot start while an op holds the actor. Lease lifetime: the actor
holds the lease while there are **subscribers OR pending work OR within a short idle
TTL**; otherwise it transitions `Closing` → releases the lease → `Closed`; the next op
re-opens (`Opening` → reacquire → replay). Races handled by the actor:
- A send arriving as the actor is `Closing` → it waits/re-opens, never races a half-torn
  state.
- Two concurrent re-opens of one lineage → the actor registry admits exactly one
  `Opening` (the other awaits it).
- A subscriber across a release → its route is closed on `Closing`; it resubscribes from
  its durable cursor on re-open.
**Defense-in-depth:** WAL append **rejects a stale lease epoch** (the file WAL currently
stamps the fence but does not reject — a draining old actor must not be able to append
after a new one took the lease). This is the WAL analogue of the vault's epoch-CAS write.

## 13. Cancel token + provider watchdog (core change)

Add a **`CancellationToken`** into `run_loop_with_store`, checked at turn boundaries and
around dispatch → `Interrupted` → `RunFinished{Interrupted}` at a safe WAL boundary
(cooperative; the durability core's fail-to-doctor — never a false `Cancelled`). Add a
**per-run provider-call timeout/watchdog** (today the model-stream await is unbounded and
pins the lease until process death; the tool path already has a 10s timeout) that
cooperatively cancels at a safe boundary, releasing the lease.

## 14. run.status enum + durable run index + busy-vs-stale

- **State enum:** `Idle | Active{run_id, step} | Paused{run_id, reason} | Interrupted{run_id} |
  Terminal{run_id, reason} | Error{run_id, detail}`. `run.status(session)` resolves the
  in-process active run first, then WAL inference.
- **Durable run index** in `store.db`: `run_id → {session, lineage, submission_id, state,
  wal_path}` so run-keyed ops survive restart. Resume **reuses the original run_id**
  (today it mints `req.run_id`).
- **Busy-vs-stale:** `session.send` on a live-active lineage returns `session_busy{run_id,
  state}` vs `lease_stale`.

## 15. session.read consistency + pagination

`after`/`head` are durable cursors (§7, WAL-seq based). `session.read` reads
`store-prefix ⊕ WAL-tail` so it **includes the in-progress turn**; `head = max(store
watermark, live tail)`, documented as moving during an active step. `limit` + `next`
cursor; chunk large transcripts (subc per-frame cap + close-on-full-egress make an
unpaginated huge read pathological).

## 16. Subscribe to a terminal run

A run terminated with zero subscribers emitted no live terminal. So `session.subscribe`
to a terminal run **replays the projection INCLUDING a synthesized terminal** (from the
durable `RunFinished`/`RunPaused`) as the final event, then closes the route. The subc
`StreamEnd` *frame* (route close) is distinct from the `ControlUnit::RunFinished`
*payload* (the projection emits the payload).

## 17. Auth failure mid-run = RunPaused, resumable (the B4 fix)

A mid-run `needs_reauth`/revoke writes a **NON-terminal durable `RunPaused{run_id,
reason: AuthRequired, error}`** record (NOT `RunFinished` — replay treats `RunFinished`
as terminal/no-op, so a "resumable terminal" would seal the run). Replay gains a
`ReplayResolution::AuthRequired`: the episode is **paused, not finished**; after re-auth,
resume **continues the SAME run_id from the last durable boundary**; no new episode starts
until the pause is resolved or cancelled. The error detail is persisted for `run.status`.
(Auto pause→refresh→continue is deferred; the lineage is preserved + manually resumable.)

## 18. WAL enrichment + fork origin (real, now)

`RECORD_SCHEMA_VERSION` bumps. Record changes:
- `RunStarted` gains `origin: Fresh | SeededFrom{source_lineage, at_seq}` (default
  `Fresh`; `seed_from` reserved on the wire, v1 rejects non-Fresh — the fork seam is real,
  cross-lineage read is future) and `submission_id` (§11), and is run_id-authoritative.
- Replay-fidelity additions: a `StepStarted` marker; `finish_reason` on
  `ModelStepFinished`; `tool_name` on `ToolDispatchIntent`; typed `error: Option<ProviderError>`
  on `RunFinished`.
- Queue identity: `Queued{submission_id, message}`, `Retracted{submission_id}` (wire the
  formerly-dead `SteerQueued`/`FollowUpQueued` into this shape).
- `RunPaused{run_id, reason, error}` (§17).

## 19. SubcToolPlane hardening (council #6)

Fence keyed `(session, tool_call_id)` **with eviction** on batch/run terminal; **per-route
window** resolved from the specific provider at route-open (not memoized once); **route
eviction** (TTL/LRU + GOODBYE on idle) against `u16` channel exhaustion; **route
invalidation on GOODBYE/status** (learn a dead route); document the per-module
`store.db` `Mutex<Connection>` as a scalability limit (read/write split or pool if it
bites).

## 20. Persistence + storage

```
~/.local/share/cortexkit/llm-runner/
  store.db                              ← sessions + the durable run index + projection cursor state
  wal/<project-id>/<session>.wal        ← per-session lineage WAL (sequence of runs)
  leases/<project-id>/…                 ← single-writer lease files
```
WAL = source of truth (portable unit); DB = derived projection. Resume = DB-prefix ⊕
WAL-tail.

## 21. The bounded llmr-core/WAL changes (enumerated — the core build phases)

Additive to a sound durability core; bump `RECORD_SCHEMA_VERSION`:
1. **Episode-aware replay state machine** (B1): per-`RunStarted` episode reset; prior
   episodes → prefix; last-unfinished resumable; explicit append-new-episode path.
   **`recover_run_config` MUST return the LAST active/resumable episode's
   config/run_id/counters, not the first `RunStarted`** (today it returns the first —
   correct for a single-run WAL, wrong for a lineage). Replay classification: last
   episode `RunPaused` → `AuthRequired`; last episode `RunFinished` → idle/appendable;
   last episode open → resume it.
2. **WAL enrichment** (§18): `origin`+`submission_id` on `RunStarted`; `StepStarted`;
   `finish_reason`; `tool_name`; typed error; `Queued`/`Retracted`/`RunPaused`.
3. **Durable cursor + projection** (B2): `WAL+store → Vec<ControlUnit>` keyed by
   `(wal_seq, sub_index)`; non-blocking in-process live tail; attach-barrier dedup by the
   durable cursor. **The durable head MUST be the last FSYNCED WAL seq, NOT
   `FileWal::last_seq()`** — `append(sync=false)` advances `next_seq` before fsync, so a
   client cursor must only advance past the fsync barrier (the live tail backpatches its
   events with assigned seqs once fsynced; `WalHandle::record`/`sync` must surface the
   assigned seq, which they discard today). Cursor state does NOT reset at episode
   boundaries (WAL seq is monotonic per lineage file across episodes).
4. **Durable queue + identity** (B3): wire the queue records, replay = queued − retracted
   − consumed.
5. **AuthRequired = RunPaused** (B4): non-terminal record + `ReplayResolution::AuthRequired`
   + same-run_id resume.
6. **Cancel token + provider watchdog** (§13).
7. **Mid-turn input channel:** the loop accepts follow-up turns from the coordinator.
8. **Durable run index** (§14).
9. **WAL stale-epoch append rejection** (M5 defense-in-depth).

## 22. Standalone CLI coexistence

`llmr-run` stays as-is; a single-turn CLI run is a lineage with one episode. Shared core,
two front-ends.

## 23. Thin-core fit (subc)

**Zero subc-CORE changes** (Oracle-confirmed). All `session.*`/`run.*` are opaque
management-surface RPCs; fan-out/replay/backpressure/coordinator/run-index are
module-side. The §21 changes are **llmr-core/WAL/store**, not subc-core.

## 24. Phased build plan (effort: Large — accepted)

- **Phase 1 — llmr-core episode model + WAL schema:** §21.1, §21.2, §21.7 + schema bump;
  conformance: multi-episode crash-cut resume (incl. a crash mid-episode-N with 1..N-1
  durable), and the episode-terminal-does-not-seal-lineage property.
- **Phase 2 — durable cursor/projection + queue + pause + cancel:** §21.3, §21.4, §21.5,
  §21.6, §21.9; conformance: queue retract/consume races across crash, replay-after-
  re-open cursor continuity, AuthRequired pause→resume same run_id, cancel→Interrupted.
- **Phase 3 — module front-end:** the lineage-actor coordinator (§12), the serve surface,
  fan-out + per-subscriber backpressure (§6), admission barrier (§10), durable run index
  (§14), SubcToolPlane hardening (§19), storage-descriptor wiring.
- **Phase 4 — surface completeness + e2e:** status enum, busy-vs-stale, subscribe-to-
  terminal, session.read pagination, AuthRequired surface; real-daemon + multi-client +
  re-open + multi-turn e2e as the ship gate.

## 25. Explicit deferrals

Fork UX (cross-lineage prefix read) · interrupt-steering (v1 = queue + retract) · auto
pause→refresh→continue (v1 = manual resume of `RunPaused`) · router-driven model
selection · cross-machine portability (distributed lease) · consumer-provided custom
tools.
