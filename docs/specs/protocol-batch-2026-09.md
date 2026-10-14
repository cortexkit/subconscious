# subc wire batch, September 2026 — r1


**Frame layout: unchanged.** Every item below is a JSON body field, a new channel-0 push, an SDK function, or a transport-crate helper; envelope bytes do not move.

Readers are deployed and verified before a producer omits an old field or sends a new one.

---

## 1. `BindIdentity.project_id: Option<String>` (subc-protocol)

**Consumer:** Broca (durable per-project lineage keyed on a rename-stable id).
**Producer:** harness plugins / prefrontal supplying it on `route.open`.

```rust
pub struct BindIdentity {
    pub project_root: PathBuf,
    pub harness: String,
    pub session: String,
    /// The entorhinal-registered project id (`pj-…`) for `project_root`,
    /// when the root is a REGISTERED project. Absent for implicit roots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}
```

Semantics, normative:

- **Registered only.** A producer sends `project_id` only when entorhinal
  resolved the root to a registered project. Implicit roots derive an id
  from the path and re-roll on rename; they are never sent as `project_id`.
  Absent therefore means "no stable id, key on the triple", not "unknown".
- **Consistent per session.** A producer sends it on every bind of a session
  or on none. A consumer keying durable state on it splits its own lineage
  if binds alternate; at cold start a triple-keyed bind opens a fresh empty
  lineage byte-identical to a new session, so **no consumer-side fence can
  catch a producer that drops the id later**. This invariant is the only
  brace; it goes in the doc comment and the CONSUMER-IMPACT notice verbatim.

   alternation between `Some(id)` and `None` silently forks lineage, so a producer that cannot answer consistently must answer `None` consistently.

  **Producer rule:**
  *"A producer resolves `project_id` at most once per session, before the
  session's first bind, and persists the outcome with the session;
  subsequent binds of that session send the persisted value and never
  re-resolve."* The alternating path is real and it is the resolver's
  unavailable arm at cold start: entorhinal unreachable for a session's
  first bind and reachable for its second would, under per-bind resolution,
  open that session once without the id and once with it. Resolve-once-and-
  persist holds across reconnect, worktree rebind (the worktree path is not
  the identity; the session row is) and a producer restart, because the row
  survives all three. A session that started while entorhinal was down binds
  without the id for its whole life, and that is correct.

  **Registered-only is a match arm, not a policy.** A resolver that returns
  a project id on every outcome (prefrontal's has three arms and all three
  carry one: `Resolved{via: Registered|Alias|Implicit}`, `Unavailable`,
  `Disabled`) leaks derived ids unless the producer gates on the resolution
  KIND: send only `Resolved` with `via` `Registered` or `Alias` (an alias is
  a registered project under another root; its id is the registered one and
  stable). Never `Implicit` (re-rolls on rename), `Unavailable` or
  `Disabled` (locally derived fallbacks). A producer pins this with a test
  driving every outcome shape through the bind and asserting the field is
  present for exactly the registered ones.
- **Daemon posture:** relayed verbatim on `route.bind`, unattested, same as
  `project_root`/`harness`/`session`. Consumers verify via entorhinal
  `resolve`. `ck routes` renders it in the table when present, abbreviated
  to `pj-` plus eight characters **with a visible ellipsis suffix**
  (`pj-a1b2c3d4…`, never a bare eight that reads as complete), full in
  `--json`. The suffix is load-bearing: a truncated id that looks complete
  is the bounded-page shape — someone compares two truncations, finds them
  equal, and concludes the ids match.
  The table supports comparison by eye with eight characters plus a visible
  ellipsis; automated consumers use `--json` for the full identifier.
- **Construct impact:** `BindIdentity` is a plain struct with struct-literal
  construction sites across the fleet; this is construct-breaking for them.
  The wave adds `BindIdentity::new(project_root, harness, session)` and
  marks the struct `#[non_exhaustive]` so the next field is not.

SDK plumbing: TS `routeOpen(..., { projectId })`; Rust `RouteTarget`/bind
options gain `project_id`. Both default to absent.

## 2. Drain: stop notice to the module, busy predicate, subscription-excluded quiescence

**Consumers:** Broca (seal), AFT (StreamEnd held-open subscriptions), every
module with a graceful-stop hook.

A graceful module stop is triggered by channel-0 GOODBYE, not SIGTERM; `Draining` moves the early stop notice to drain start while GOODBYE remains the exit trigger at drain end.

The described restart sequence drains routes for up to 30 seconds, emits `route.closed` and route GOODBYEs, sends the module's channel-0 GOODBYE, then waits at most 30 seconds for child exit before SIGKILL.

Three changes, landing together or not at all:

**2a. Module-side stop notice at drain start.** The daemon sends the module
a new channel-0 control command when the drain begins:

```rust
// subc-protocol session.rs, ModuleControlCommand
Draining {
    reason: RouteCloseReason,   // reload | restart | disable
    deadline_ms: u64,           // the drain ceiling, from the daemon's clock
}
```

A module receiving it must stop admitting new work, `StreamEnd` its
held-open subscription lanes, and let in-flight requests settle. It is
advisory: a module that ignores it is treated as today. The module GOODBYE
at drain end is unchanged and remains the exit trigger.

**Timing is the contract, not the frame shape.** The notice is written to
the module's channel 0 **at drain start**, before the quiescence wait
begins — never at the end of the window, and never as a courtesy before
SIGKILL. A notice delivered at the end of the window is indistinguishable
from no notice, because the work it authorises (a seal, a flush) takes
longer than what remains.

**Recommendation to implementers:** log BOTH arms of
the stop hook, including the success shape ("sealed 0, none in flight"). A
module that logs only its failure arm cannot be told apart from one whose
handler never fired; silence must be a finding, never a success shape.

**2b. Busy predicate from declared gauges.** A module may declare, in its
manifest `self_signals`, one entry of a new kind:

```rust
SelfSignalKind::Busy   // anchored_to ONE OR MORE health gauge names
```

When present, the drain's quiescence condition becomes
`wire_quiescent && sum(health.metrics[g] for g in gauges) == 0`, polled from
the module's `health.check` reply at the existing probe cadence. **More than
one gauge is allowed and they are summed by the daemon**, because a single
declared number hides which one held the drain: broca's own seal waits on
`runs_in_flight == 0 && opening == 0` (registry.rs:1104), and a session
mid-open has no run yet and is about to have one, so a gauge of runs alone
reads zero during exactly the window where admission is in flight. A
module declares both; health shows both; the daemon sums.

Undeclared modules keep wire-only quiescence. The gauge must be a
non-negative integer; anything else reads as "busy" for that drain.

**Omission is busy, counted, never refused.** A declared gauge the health
reply omits reads as busy for that drain and the daemon increments
`drains_with_undeclared_gauge` (surfaced on `server.describe` beside the
drop counters, and rendered by `ck module status`) — a COUNTER, not only a
log line, because "busy for one drain" means a module that never fixes its
declaration costs one full drain window per restart forever, and a log line
nobody greps is the shape this fleet has spent two days convicting. HELLO is
never refused on this: the fail-safe direction is correct (busy-on-omission
cannot cut work; quiet-on-omission can), and it should be diffable.

Wire credits do not prove module work is quiescent: a call can acknowledge admission before background work completes, while an idle subscription can hold a credit indefinitely; declared busy gauges and subscription exclusion address these distinct cases.

**2c. Subscription-excluded wire quiescence.** `endpoint_in_flight_count`
(forwarding.rs:1347) counts every acquired credit. The drain's quiescence
wait excludes credits whose request the **client declared a subscription at
open** — an explicit request-kind bit on the route request, never inferred.
The first reply is not a subscription discriminator: `StreamData` with no
terminal yet can also come from a `bash` call with `wait:true`
on a 25-minute cargo run holds its permit exactly the same way for exactly as
long and MUST count as in flight (it is the class `drained:false` was right
about), and a provider that streams a long tool result would be misread the
same way. There is no body field the daemon parses and no stream-begin frame
in subc-protocol, so the only honest signal is the client saying so.

Wire: a `SUBSCRIPTION` flag bit on the `Request` envelope (the envelope's
remaining reserved bit, allocated here; TS/Swift constants and the golden
`protocol_constants.json` in the same bump). The SDKs set it from
`subscribe()`/`callStreamed`-style entry points; an ordinary `call()` never
sets it. The daemon reads it at credit acquisition and tags the credit.

The excluded set is **settled at drain start**: captured when the drain
begins; subscriptions opened after the notice are not excluded (they should
not exist; 2a asked the module to stop admitting). Excluded subscriptions
still receive their route GOODBYE at drain end as today.

`route.closed` gains `excluded_subscriptions: u32` beside `drained` and
`abandoned`, so a consumer can tell "drained because nothing was in flight"
(0) from "drained with N held-open subscriptions excluded" — the line that
says whether a consumer's wake lanes were live at close.

Ordering with 2a: the stop notice is written to the endpoint's channels
**before** the quiescence wait begins, so a consumer's cancel-on-closing
fires before any GOODBYE. If a drain completes early under 2c, the consumer
sees the notice, then GOODBYE — never GOODBYE alone. (A consumer that treats
GOODBYE as terminal regardless is safe either way; the stop-first order is
the cleaner one and is normative.)

The 30 s ceiling is never extended by any of the three. Consequently `Busy`
changes **when** the kill lands inside the ceiling, never **whether**: a
module whose gauge is still non-zero at 30 s is killed as today. A module
declares `Busy` to protect work that settles inside its own deadline
contract, not to hold the daemon.

Seal rules a module's stop hook must satisfy:
interrupted work is `Interrupted`, never `Cancelled`; a seal that drops the
indeterminate set is worse than none; when fsync cannot land, exit unsealed
and say so; **the success path logs too**, including "sealed 0 in flight" —
silence must never be a success shape.

## 3. `call_with_streamed_body` (SDK affordance; no daemon change)

**Consumer:** Magic Context. Streamed bodies carry a `state_sync` cold seed for a previously unseen session or `lineage.begin/put/finish` uploads; that lineage protocol already chunks data above 1 MiB with its own begin/put/finish semantics. This SDK affordance remains available without a daemon change.

Facts: `MAX_FRAME_BODY_LEN` is 64 MiB (subc-protocol lib.rs:162). The
client→module data path is `route.module_sink.send(frame).await`
(router.rs:665): it **awaits** writer capacity, so a client streaming
chunks gets TCP backpressure through the daemon, never a drop. The
module→client direction is `try_send` with connection-close escalation on a
full sink (router.rs:450): a module streaming replies to a slow client is
the arm that drops. The spec below is client→module.

Shape: one `Request` frame carrying the JSON head (`op`, params, and
`streamed_body: { total_bytes?: u64, digest?: "sha256:…" }`), followed by
`StreamData` frames on the same `corr` carrying opaque bytes, terminated by
`StreamData` with the `LAST` flag. The module SDK reassembles against a
per-op ceiling the module declares in its manifest operation:

```rust
ManagementOperation {
    …,
    streamed_body_max_bytes: Option<u64>,
    streamed_body_digest: StreamedBodyDigest,   // Optional (default) | Required
}
```

The ceiling counts **body bytes only** — the head `Request` frame is bounded
by `MAX_FRAME_BODY_LEN` like any request and does not count against it.

Refusals by name, before any bytes are buffered past the ceiling:

- `streamed_body_over_cap` fires on **two arms**, both tested: (i) EARLY, on
  the head, when `total_bytes` is present and exceeds the ceiling — before
  any `StreamData` is read; (ii) LATE, on actual bytes crossing the ceiling,
  when `total_bytes` is absent or understates the body. A head that lies
  small does not buy a larger buffer.
- `streamed_body_unsupported` (op has no ceiling declared).
- `streamed_body_digest_required` (op declares `Required` and the head
  carries no digest) — refused on the head, before buffering. A module whose
  custody model is digest-bound (D5's begin/put/finish refuses a conflicting
  re-declaration with the original intact) cannot admit a body it cannot
  bind, and the SDK refusing beats the handler discovering it after
  reassembly.
- `streamed_body_digest_mismatch` (trailer digest present and wrong).
- `streamed_body_sender_closed` — the sender's connection closed, or the
  request was cancelled, before the `LAST` frame. Surfaced to the handler as a
  typed refusal on the reassembly, never as a hang: an abort after the head
  must not leave the module reassembling a body nobody will read.
  `on_stream_begin` admits on the head; this is the symmetric close.

No resume after a dropped chunk: under backpressure-not-drop a dropped chunk
is a dropped connection, and the consumer re-sends the whole body (Magic Context's
bodies are idempotent by key — `state_sync` on ordinal/inventory, D5 uploads
on digest). One trailer digest over the whole body, not per chunk: these
bodies are consumed whole.

An `on_stream_begin` admission hook lets a module refuse on the head alone. Rust `SubcConsumer::call_with_streamed_body(target, head,
impl AsyncRead)`; TS `callStreamed(moduleId, head, ReadableStream)`. The
binary-body gap noted in Rust (`send_request` hardcodes `Flags::new(false,…)`)
and Swift (`beginRouteRequest` binary:false) closes in the same wave.

**Deferred from this wave:** this affordance requires per-operation
`streamed_body_max_bytes` and `streamed_body_digest` fields on
`ManagementOperation`; those fields do not exist at
`crates/subc-protocol/src/manifest.rs:1473-1480`. It is therefore a wire change,
not an SDK-only affordance, and belongs in a later versioned wave with its own
crate bump and fleet notice.

## 4. `connection_file::discovery_candidates` treats empty as unset (subc-transport)

`discover()` already filters `SUBC_CONNECTION_FILE=""` via
`non_empty_os_var`; the ladder function `discovery_candidates(explicit,
env_named)` does not, so a caller passing `Some("")` gets `PathBuf::from("")`
(a bare relative filename, i.e. cwd) as its only candidate. The filter moves
into the ladder for both `env_named` and `runtime_dir`; test: `Some("")`
behaves as `None` and every candidate is absolute.

## 5. `pub const WIRE_CRATE_VERSION: &str = env!("CARGO_PKG_VERSION")` (subc-protocol)

**SATISFIED, UNDER A BETTER NAME:** the const already exists as
`SUBC_PROTOCOL_CRATE_VERSION` and already serves this purpose; six sibling
repos import it. The name this item proposed is worse, not just different: four
crates in this workspace are wire crates, so `WIRE_CRATE_VERSION` *inside
subc-protocol* reads as "the version of the wire crates" rather than "the
version of this one". Keeping the precise name, adding no alias — two spellings
of one const is a surface to keep synchronised for no gain.

For `ManifestProvenance.wire_crate_version`; Plexus and Broca refuse to
hand-type it.

## 6. Daemon logging `with_ansi(false)` (subc-core, same window)

**SATISFIED:** the daemon's `cortexkit_log::init` path renders `level.as_str()` and strips ANSI; `daemon_log_line_matches_the_authority_fixture_byte_for_byte_without_ansi` is the standing real-path proof.

Not wire. The non-tty daemon log carries ANSI styling that breaks contiguous
ASCII matching on level words; the stall-hunt queries that pinned the
current rendering are closed. Lands in the daemon cut that carries 2a–2c.

---

## Public presentation and refusal requirements

Swift SDK `Draining { reason, deadline_ms }` must not appear to the app as `FedConnectionState.disconnected` or a retryable failure; invisible presentation or a distinct waiting state is acceptable, but a healthy drain must not look like a connection fault.

Undeclared drain gauges make that drain busy, are counted as `drains_with_undeclared_gauge`, and never refuse HELLO; several gauges are summed.

Trailer digests are optional by SDK default, verified when present, and required by an op declaring `streamed_body_digest: Required`; an absent required digest refuses on the head with `streamed_body_digest_required`.

`ck routes` shows a present `project_id` in its table as `pj-` plus eight characters and a visible ellipsis (§1); `--json` returns the complete identifier so callers do not compare truncations as if they were full IDs.
