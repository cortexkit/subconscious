# Module readiness and blue/green swap


## The ladder

Each rung is independently shippable and each later rung needs the earlier
one.

### Rung 1 — reader head-of-line fix (done, unplaced)

Bounds a slow module's blast radius to its own callers. Not part of this
design; listed because rungs 2 and 3 are wrong to build without it, since
without it a "not ready" module that is nevertheless receiving relays still
starves everyone.

### Rung 2 — module-declared readiness

A registered module may declare itself **not ready**. While it is not ready,
`route.open` targeting it is refused with the existing `module_warming`
code — already typed, already retryable, already bound to the SDKs' 30 s
route-open deadline — instead of being relayed.

Wire:

- `ModuleManifest` (HELLO) gains `ready: Option<bool>`, default `true` when
  absent. Absent is "ready", so every deployed module keeps its current
  behaviour with no edit. Constructed through the builder, so no construct
  site breaks.
- `catalog.update` gains the same optional field, so a module can flip
  `false -> true` when its setup completes without re-registering.
- `catalog.list` mirrors it, so `ck catalog <id>` and consumers can see it.

Daemon:

- `ModuleRegistration` carries `ready: bool`.
- `handle_route_open`: after `get_module` succeeds and before any relay,
  `if !registration.ready { refuse module_warming }`. Same wire code, so
  nothing new for clients to classify. NOT the same log line or counter
  key: today's `module_warming` is emitted only by
  `supervised_absent_route_open_refusal_frame`, which stamps
  state/enabled/live from the supervisor snapshot (control.rs:2108-2135);
  a registered-not-ready module has no such status. Reusing that builder
  would make "absent" and "declared not ready" indistinguishable in the
  counter and in the log — on exactly the incident class this rung exists
  to diagnose. So: same `code`, a distinct message, `ErrorBody.detail =
  {"reason": "declared_not_ready"}` (the field exists and is `None` on
  other paths), and a distinct log field `reason=declared_not_ready`.
  The closed counter vocabulary gains one key for it.
- **Best-effort, not an invariant.** `handle_route_open` reads readiness
  under one lock and reserves the relay under another; a module can flip
  `ready` between the two, and under rung 3 the check can read the
  pre-cutover active while the reservation lands on the post-cutover one.
  So a module MUST still tolerate an `on_bind` while not ready. The rung
  removes the common case, not the possibility; document it that way on
  the manifest field.
- The supervisor's health probe is unchanged. Readiness is orthogonal to
  health: a module can be healthy and not ready (warming), or ready and
  degraded. Conflating them is why Kubernetes has two probes; we already
  have the liveness half.

Module obligation (aft, first adopter): pre-warm at start from a persisted
root list rather than on first bind, register with `ready: false`, flip to
`ready: true` when the persisted set is warm. **Without this the rung is
inert** — a process that receives no traffic never warms, and readiness
would just be a longer version of today. This is aft's work and is named as
a precondition, not assumed.

What rung 2 changes for callers during a restart: today they see 12 s bind
stalls (or, with rung 1 placed, fast `module_timeout` after three). With
rung 2 they see fast `module_warming` from the first call, retry inside
their deadline, and land on a module that is actually ready. The window is
the same length; every call inside it is cheap and honest.

**How long a not-ready window callers will actually wait.** Rust consumers
retry a retryable refusal until `route_retry_deadline` (30 s) or the call's own
deadline, whichever is sooner; that path has no attempt cap. Node consumers
that open routes through the managed path (`client.call()` and managed
requests, which go through `openCachedRoute`) do the same on
`@cortexkit/subc-client` 0.11.0 and later. On an older client that path gives up
after about 3.1 s: a six-attempt cap (0/100/300/700/1500/3100 ms) sat beside
the deadline until 388536cb made the deadline the only binder, and a 0.x caret
pins the minor (`^0.8.1` never resolves 0.11), so those pins keep the old
behaviour until the range is edited. A module declaring `ready: false` for
longer than ~3 s will fail such consumers' opens outright. A consumer that
calls `routeOpen()` directly gets one attempt and the refusal back, and its own
loop decides how long to wait whatever the SDK version: aft's bridge retries
for up to 15 s.

**Whether warming is worth it at all is the module's measurement.** aft
measured it against a burst of 144-170 first calls over 24 real roots and
found warm-then-flip slower at both p50 (1234 to 2855 ms) and p99 (2485 to
4178 ms): none of its first calls waits on a decoded artifact, so the refusal
window was pure added latency. aft flips ready at once. The comparison that
decides it is first-answered-call latency across every reopening client, at
p50 and p99, not one call timed either way.

### Rung 3 — blue/green swap

`supervisor.swap { module_id }` (and `ck module restart --swap <id>`):

    1. spawn CANDIDATE alongside INCUMBENT       (candidate slot: own child,
                                                   own nonce, own cgroup and
                                                   capture-file suffix)
    2. candidate registers with ready:false       (candidate slot in registry
                                                   AND forwarding; unroutable)
    3. candidate warms, flips ready:true          (rung 2 signal)
    4. CUTOVER: swap active/candidate slots       (one forwarding write lock;
                                                   generation bumps)
    5. incumbent drains BY ENDPOINT: route.closing,
       quiescence wait, route.closed, GOODBYE      (reason `Restart` on the
                                                   wire; see below)
    6. incumbent exits; candidate slot freed;
       registry/forwarding candidate := None

Failure arms, which are the point:

- Candidate never registers, never flips ready, or fails its first health
  probe within a budget: **kill the candidate, incumbent untouched, swap
  reported failed with the candidate's terminal record**; candidate spawn must leave the incumbent's nonce, cgroup, capture file, and registration untouched.
- Candidate registers but the incumbent dies during the swap: promote the
  candidate immediately (it is the only live process); if it is not ready,
  callers get `module_warming` as in a plain restart.
- Daemon restart mid-swap: the swap is not durable; both children observe
  EOF and exit; next boot spawns one child as today.

What callers see: bound routes on the incumbent get `route.closing` /
`route.closed` / `GOODBYE` at step 5 exactly as on a restart today, because
sessions are module-local. Their reopen lands on a **warm** process. The
unavailability window collapses from "warmup time" to "one reopen round
trip".

### Swap admission and cutover requirements

Candidate registration must not replace the active endpoint before cutover; by-id routing continues to resolve the active slot.

Retirement waits track the incumbent's endpoint or process slot, not the shared module ID: the candidate keeps that ID registered after cutover, so waiting for the ID to disappear would never finish and draining by ID would drain the replacement.

A superseded incumbent's bind acknowledgement releases its reservation, answers the waiting client with retryable `module_reloading`, sends GOODBYE on that channel, and leaves the incumbent's other routes alive.

Before the ordinary reserved-ID gate checks a registering process's identity, candidate HELLO admission constant-time compares its nonce with the candidate nonce minted by the supervisor, for reserved and unreserved IDs alike; missing or invalid tokens are refused so lifting duplicate-ID refusal cannot admit an unsolicited replacement.

Swap closes routes with `Restart`; `RouteCloseReason` remains a closed set.

`ModuleSpec.overlap` declares whether two instances may safely coexist (see “Who can use rung 3”); it is read from daemon configuration and cannot be widened by `catalog.update`. Swap requires a registered incumbent, otherwise it is refused.

### Who can use rung 3

**The test for declaring `overlap: safe`:** a second instance of the module can
run for a few seconds without touching any single-writer store, meaning every
database, WAL, index, projector and scheduled job it owns. A lease on part of
that state does not qualify it; the declaration must account for every writer, including indexes, projectors, and scheduled jobs.

So rung 3 is **opt-in by manifest declaration**:

    overlap: "exclusive" | "safe"        default "exclusive"

`supervisor.swap` on an `exclusive` module is refused with a typed error
naming the declaration. `ck module restart --swap` on one prints the same
and suggests plain restart. Making a module overlap-safe is module work; the daemon only guarantees it never overlaps a module that has not declared it safe.

### The warm set: which roots a candidate should load

The daemon's bound and pending routes define the live root set; persisted module history is not a substitute for that snapshot.

**Daemon obligation 1: keep the root.** Store the canonical root on
`RouteBinding` and on the pending reservation, as the `ProjectRootId` the
open already computed. The type is `Option` on purpose. A binding created
before the daemon carrying this change has no root, and it must stay
*unknown*, never become *absent* (see below).

**Daemon obligation 2: the query.** A new channel-0 op:

    request   ModuleControlRequestFromModule::LiveRoots {}
    reply     LiveRoots {
                module_id,
                roots: [ { project_root, bound, pending } ],   // counts per root
                unknown_root_bindings,                          // bound + pending with no stored root
                total_bindings,
              }

The reply has to distinguish three answers, and it does so by construction.
The following three reply states keep unrecorded roots distinct from an empty route set:

| roots | unknown_root_bindings | meaning | candidate does |
| --- | --- | --- | --- |
| non-empty | any | these roots are live | warm them under the budget |
| empty | > 0 | routes exist, roots unrecorded | warm lazily; this is NOT "nothing to warm" |
| empty | 0, with `total_bindings == 0` | no routes at all | flip ready at once |

If pre-change bindings were simply left out, "unknown" and "none" would be
the same empty list. The first swap after the daemon cut would then warm
nothing, flip ready immediately, and look exactly like a successful warm
swap: a cold cutover that reports itself as fine. `total_bindings` exists so
the third row is a positive statement, not an inference from two zeros.

The query is module-originated on the caller's own module connection, with no module ID in the request; the daemon answers for that registration only, so the module cannot enumerate another module's routes.

Snapshot contract: resolve the routable endpoint and count bound and pending
bindings under one forwarding read lock, the same linearization point
cutover's write lock uses, so a reply describes either the incumbent before
cutover or the candidate after it, never a mix. The reply must satisfy
`total_bindings == sum(bound + pending) + unknown_root_bindings`.

**Swap semantics.** Incumbent and candidate share a module id, so the query
needs a rule for which one it describes. It reports the bindings of the
endpoint that is routable when the query is answered. Before cutover that is
the incumbent, which is the set that is about to move and so the one worth
warming. The reply is a snapshot. Routes the incumbent takes after the query
are not in it, and they warm lazily after cutover, as every root does today.
A candidate may re-query before flipping. It has no reason to poll.

**Where `module_warming` is visible.** During a swap, the candidate's
not-ready state is invisible to callers: the incumbent stays routable until
cutover, so nobody's `route.open` is refused while the candidate warms. The
warm-up budget costs callers nothing in the swap case. It costs them only on
a plain restart (no incumbent), where route.open answers `module_warming`
for the whole budget. The SDKs retry that code only until their route-open
retry deadline, 30 s by default. **So on a plain restart the budget must
stay well under 30 s, or opens fail rather than wait.** The budget value
itself is the module's, measured from per-root load times. It is a module
constant, not daemon config, because only the module knows what one root
costs; separate restart and swap budgets may be used because callers wait through a plain restart but the incumbent serves during swap warming.

**Telling a swap from a restart.** Two budgets only work if the module
knows which case it is in, and it has to know before HELLO, since that is
when it plans its warm-up. The supervisor decides this at spawn, so the
signal is an environment variable set on the candidate's spawn:

    SUBC_SPAWN_ROLE=swap_candidate

It is absent on every other spawn, and **absence means plain restart**, the
safe reading: a module that misses it uses the short budget and gives up
some warmth, never correctness. Of the three possible carriers it is the
only one available before HELLO, and it needs no wire change. It says only
which budget applies. The variable is not authority: the nonce check at HELLO, not the environment hint, proves the candidate was supervisor-minted.

### Overlap requirements

When a bind acknowledgement targets the incumbent after cutover, detect that superseded endpoint before removing the pending relay's client reply sender and channel reservation; those records are needed to return `module_reloading` and close the abandoned channel without ending the incumbent connection.

Catalog updates and deregistration identify a registration by connection ID; both operations must search the candidate registration as well as the active one so a warming candidate can update `ready` and clean up its registration.

Consumer attestation accepts the incumbent and candidate slots' nonces while the swap is open.

Candidate exits use a separate reap path that releases only the candidate's slot and nonce without spending a restart-budget unit or changing the incumbent's state.

Plain spawns remove `SUBC_SPAWN_ROLE`, and the variable is refused as a configured `env` key.

### Swap behavior

The swap runs inside the module's supervise loop, like a restart
(`supervise_swap.rs`). `supervisor.swap` is answered at cutover or failure;
the incumbent drains after the answer, so a caller whose own lane rides the
incumbent is not waiting on a drain that waits on it.

While the candidate warms, the loop keeps serving the module's commands. A
stop, disable or retire aborts the swap (arm `interrupted`): the candidate is
killed on its own reap path with no budget spent, and the command then runs
on the incumbent. Restart and reload are refused `swap_in_progress`; a config
update is answered at once and applied when the swap ends. The control
handler holds the daemon-wide operation lock only to look the module up, not
across the swap, since `ck module stop` takes the same lock. After cutover,
while the incumbent drains, commands wait as they do behind a plain restart's
drain (at most the drain budget).

The candidate's admission and promotion follow these rules:

1. HELLO: a swap gate runs before the reserved gate. While a swap is open for
   an id, the candidate's nonce registers through `register_candidate_*`
   (registry, then forwarding, the ordinary HELLO's order); the incumbent's
   own nonce falls through to the ordinary gates; anything else, including no
   nonce, is refused `swap_token_invalid`. The token admits one registration.
2. Readiness is read from the candidate's registration by its connection. The
   one health probe of the candidate (sent only if it advertises
   `health.check`) is endpoint-keyed, as is the busy-gauge probe of the
   draining incumbent.
3. `route_census`, `live_roots` and `catalog.list` show only the
   promoted process after cutover; the incumbent's draining routes still get
   `route.closing`/`route.closed`, and its routes are gone within one drain
   budget.
4. Ordinary registration of an id with an open swap is gated
   by the swap token, which covers the window where it matters.
5. The incumbent never queries `LiveRoots` after cutover in this
   sequence.
6. Forwarding cutover first, then registry promotion. Every route.open reads
   the registry and then reserves under the forwarding write lock, and the
   reservation alone picks the process: in the gap a route.open reads the
   incumbent's registration and lands on the candidate, which is ready and is
   the process the module now is. With this order the registry never names a
   process forwarding does not already route to. If the candidate vanishes
    between the two calls, the incumbent cannot be restored, so the swap reports `cutover_lost` and falls back to a plain restart; in this fallback the incumbent's routes close as a crash rather than drain.
7. The incumbent is retired by its own
   function, which drains with `begin_endpoint_drain` (reason `Restart`),
   reaps the process, and waits for release by `Connection`. The module's
   state is not touched, since it describes the promoted candidate.

The candidate gets the same health probe once
before cutover (`failing` or no usable answer fails the swap, `degraded` does
not). No swap failure spends a restart-budget unit: a failed candidate is
classified and reaped on its own path and never reaches `next_crash_restart`.

If the incumbent's registration goes away while the candidate warms, the
candidate is promoted as soon as it has registered, ready or not.

Promotion runs the control plane's registration bookkeeping that the
candidate's HELLO skipped (capability cache, deny census, requirement
recompute), through a promotion observer the router installs on the shared
supervisor handle. `server.describe` recomputes requirements from the live
registry on every read, so what promotion changes observably is the cached
manifest, which answers while the module is enabled but unregistered.

- `overlap` lives in `ModuleSpec` and `subc.jsonc` only; `catalog.update` carries no overlap field, so it is frozen by construction.
- Both processes of a swap write `<id>.stderr.log`, the file `ck module logs`
  reads; the daemon appends whole lines, so the overlap interleaves by line.
  The cgroups must differ, alternating between two names while the promoted process keeps its name; the encoding is injective (every byte outside `[A-Za-z0-9.-]`, `_` included, becomes `_xx`, and the alternate name adds `_swap`) so distinct module and swap names cannot alias a directory.
- The candidate emits no spawn event until cutover (the spawn feed holds one
  live process per id); the superseded incumbent's exit is emitted for its own
  generation without clearing the promoted process's entry. Its stderr goes to
  the module's one stderr ring, marked by a process-start boundary.
- The default readiness budget is 100 s (aft's 90 s swap ceiling plus start
  and HELLO); `ready_timeout_ms` overrides it per request.

### What rung 3 does not do

- No transparent route migration. Routes close and reopen.
- No swap across a daemon restart.
- No automatic swap on crash. The restart budget path is unchanged; swap is
  an operator or placement-tool verb.
- No health-based automatic rollback after cutover. If the candidate goes
  bad after promotion, that is a normal unhealthy module and the existing
  restart policy applies.

## Readiness and overlap rules

1. `module_warming` belongs to the closed retryable set in the protocol crate and both SDKs; unknown codes are terminal, and readiness versus absence is distinguished in the message, `ErrorBody.detail`, and a log field, never in the code.
2. `ready` may go `true -> false` after registration; the check is
   best-effort per route.open and costs nothing. A module flapping it
   looks like a restart storm to callers, which is that module's defect.
3. Overlap is a manifest opt-in, default exclusive, frozen on
   catalog.update, sourced from ModuleSpec so it is readable when the
   module is down.
