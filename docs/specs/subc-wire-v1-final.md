# subc Wire v1 — Final Envelope Revision

Status: DRAFT — pending blind Oracle gate, then flip-day build.
Decided in channel `#subc-wire-v1-final` (all six seats: SUBC, AFT, MC, ALF, BROCA, FED + Ufuk), 2026-07-12.

## 0. Why this revision exists, and why it is in-place

Nothing public consumes the subc wire. Every consumer/producer is first-party
(the supervised fleet + the three client libraries). This is therefore an
**in-place revision of the v1 layout**, not a versioned migration: no dual
codec, no version negotiation, no compatibility machinery. The whole fleet is
rebuilt and restarted in one coordinated cutover (§9).

This is the last free window: once a public consumer exists, the envelope is
versioning-forever and in-place changes stop being possible. The round
inventoried every seat's envelope-level needs (§8 records the negative space)
so the layout is final.

**Driver:** routing-audit BLOCKER — the daemon reuses a released module-local
channel after the u16 cursor wraps; a late frame for a closed route can then
misdeliver into a different tenant's route (cross-tenant body exposure, credit
corruption). Client-side mitigations shrink the window; only a wire-level
epoch eliminates the class.

## 1. Envelope layout

Fixed **21-byte** header (was 17), little-endian:

| Offset | Field   | Type | Notes                                        |
|--------|---------|------|----------------------------------------------|
| 0..4   | len     | u32  | body bytes after the header (unchanged)      |
| 4      | ver     | u8   | **2** after this revision (see §2)           |
| 5      | ty      | u8   | FrameType (unchanged enum, 0..=11)           |
| 6      | flags   | u8   | see §5                                       |
| 7..9   | channel | u16  | route slot, sender-local space (unchanged)   |
| 9..13  | epoch   | u32  | **NEW** — per-slot binding epoch (§3)        |
| 13..21 | corr    | u64  | correlation id (unchanged)                   |

- `HEADER_LEN` 17 → 21. `FROZEN_PREFIX_LEN` stays 5 (`len`+`ver`), and the
  frozen-prefix invariant is preserved verbatim: any receiver of any future
  version can always read `len` and `ver`. This revision slots `epoch` between
  `channel` and `corr`; nothing moves inside the frozen prefix.
- Channel stays **u16**. Decided on ALF's capacity fact (concurrent-live
  routes per shared connection: tens–low hundreds realistic, low thousands
  pathological — two orders under 65,536) plus the durable escape hatch that
  channel space is per-connection (a hypothetical >65k shape shards to a
  second connection, zero wire change). u16 keeps the daemon's hot-path
  dispatch a flat O(1) array; u32 would force hashmap lookup on every relay
  forever.
- Epoch is **u32** because in the u16-channel regime slots are
  scarce-and-reused: the epoch is hot load-bearing, and u32 depth (§3.4
  retires a slot before wrap) removes wrap from the analysis entirely.

## 2. `ver` byte: stale-binary tripwire (NOT version negotiation)

The `ver` byte increments 1 → 2. `PROTOCOL_VERSION = 2`,
`MIN_SUPPORTED_VERSION = 2`. There is **no** v1 codec, no negotiation, no
dual-path anything — exactly one supported version, as before.

Rationale: the frozen prefix keeps `len`+`ver` readable across layouts. A
stale binary that missed flip-day would otherwise desync framing (it writes
bodies at offset 17; the new reader consumes 21) and fail as confusing garbage
mid-stream. With the byte bumped, the very first frame from a stale binary
fails loud and named: `DecodeError::UnsupportedVersion { ver: 1 }` — an
actionable "rebuild this binary" signal instead of a framing-corruption hunt.
One constant, zero machinery. This is a deliberate deviation from the room
synthesis (which kept ver=1), flagged for the gate.

## 3. Per-slot binding epoch

### 3.1 Semantics

The epoch counts **the Nth binding of a specific channel slot**, per side:

- Client side: per `(client_connection, client_channel)` slot.
- Module side: per `(module_endpoint, module_channel)` slot.

`(channel, epoch)` is always expressed in the **sender's local space**, like
`channel` already is. The daemon rewrites **both** fields on relay (it already
rewrites `channel`; `epoch` is rewritten alongside from the binding). One
route binding therefore has two epoch values — the client-slot epoch and the
module-slot epoch — each meaningful only in its own space. Neither side ever
sees or reasons about the other side's epoch.

Epoch `0` is **reserved**: it is the fixed epoch of channel 0 (the control
channel, which is never rebound) and is never assigned to a route binding.
The first binding of any route slot gets epoch `1`.

### 3.2 Assignment and distribution

- The daemon owns all epoch counters, exactly as it owns channel allocation.
  Per connection (client side) and per endpoint (module side) it keeps a
  lazily-populated `slot → last_epoch` map; binding a slot increments it.
- `route.open` response (`ClientControlResponse::RouteOpen`) gains
  `route_epoch: u32` next to `route_channel`.
- `route.bind` (`ModuleControlRequest::RouteBind`) gains `epoch: u32` next to
  `route_channel` (the module-slot epoch).
- Both sides stamp their `(channel, epoch)` pair into every frame header on
  that route. Channel-0 frames carry `epoch = 0`.

### 3.3 Validation (direction-agnostic, per-frame)

At relay, for every data-plane frame (`lookup_data_route` ingress, both
directions):

- Binding found for `(connection|endpoint, channel)` **and**
  `frame.epoch == binding.<side>_epoch` → forward (rewriting channel+epoch).
- Binding found, epoch mismatch → **DROP** the frame (debug log + counter,
  never an Error frame — erroring would inject into the *new* binding's corr
  space, which is the confusion the epoch exists to prevent). This is the
  stale-frame kill: late frames from a previous binding of a reused slot can
  never reach the new tenant.
- No binding → unchanged existing behavior (`unknown_channel` Error for
  Requests, silent drop otherwise).

Direction-agnostic is load-bearing (AFT's pin, ALF concurring): reverse-lane
Requests (module→client elicitation/sampling, `execute_effect`) are validated
identically — a consent prompt surviving a rebind onto a new epoch is a
cross-tenant delivery. Pure-header frames (`Cancel`, `Goodbye`, `Ping`,
`Pong`) carry and are validated on epoch like any other frame; a stale-epoch
`GOODBYE` is dropped, which also closes the late-GOODBYE-tears-down-the-new-
binding race by construction. Daemon-originated frames (cleanup GOODBYEs,
module-death propagation) are stamped with the target binding's current epoch.

### 3.4 Allocator interaction and wrap

- The channel allocator never assigns a slot with a live binding (existing
  behavior, now stated as an invariant); released slots are freely reusable —
  the epoch disambiguates generations.
- A slot whose epoch reaches `u32::MAX` is **retired** for the lifetime of its
  connection/endpoint (allocator skips it). Wrap is thereby impossible by
  construction, not merely improbable. (4B rebindings of one slot within one
  connection lifetime is unreachable; the rule exists so the analysis is
  closed, not statistical.)
- Epoch counters die with their connection/endpoint — cross-connection
  staleness is already impossible (connection identity separates spaces).

## 4. What the epoch replaces

The alternative was daemon-side quarantine-retire of recently-released
channels (probabilistic: shrinks the window, leaves a residual for buggy
same-host modules). Rejected in favor of the structural fix, per the
first-party-fleet principle (§0). No quarantine code ships.

## 5. Flags byte (final)

| Bits | Field           | Semantics                                          |
|------|-----------------|----------------------------------------------------|
| 0    | BINARY          | unchanged                                          |
| 1-2  | PRIORITY        | unchanged: relay-QoS **ordering within a class**; 0b11 decode-rejected |
| 3    | LAST            | unchanged                                          |
| 4-5  | ADMISSION CLASS | **NEW** mutually-exclusive field, see below        |
| 6-7  | reserved        | must be zero, decode-rejected (unchanged rule)     |

Admission class (one 2-bit field, not two independent bits — per-frame the
states are exclusive and `EXPEDITE+SHEDDABLE` dies by construction):

- `00` NORMAL — must-deliver semantics exactly as today.
- `01` EXPEDITE — admission/relay priority **hint**: the daemon MAY relay
  sooner; the receiver MAY admit sooner. Never affects delivery guarantees.
  Known consumers: bind-priority, health probes, room wake hints,
  interactive-vs-batch embedding.
- `10` SHEDDABLE — the daemon MAY drop this frame under egress pressure
  instead of escalating (today's behavior on full client egress is
  close-the-connection; a sheddable frame is dropped without closing and
  without credit implications). **Legal only on `Push` and `StreamData`**;
  any other frame type with class `10` is decode-rejected
  (`DecodeError::SheddableIllegalFrameType`). Sender contract is
  skip-tolerant; recovery is consumer-level (cursor replay / skip-and-log).
  Terminal frames (`StreamEnd`, `Error`, `Response`) are structurally never
  sheddable, so streams always end or the connection closes. Known
  consumers: broca display-lane deltas (lossy by contract), ALF room hints
  (design-lossy, poll-floor recovery), future WAN/Cortex-App telemetry
  pushes. MC's 26GB shadow-mirror retention incident is the *rationale* for
  daemon-level shed semantics existing, but the mirror lane itself is NOT a
  consumer: it sends Request-shaped frames (which stay on client-side
  bounded-queue discipline, already shipped) and the lane is temporary by
  design. Request-shed is deliberately not offered — it would require
  synthetic-error machinery in the daemon.
- `11` reserved-invalid — decode-rejected (`ReservedAdmissionClass`), same
  posture as priority `0b11`.

Composition rule (BROCA's sentence, normative): the admission class governs
**admission/shed only** and composes orthogonally with PRIORITY, which orders
**within** a class. An expedited frame is ordered among expedited frames by
PRIORITY. SHEDDABLE remains illegal outside `Push`/`StreamData` regardless of
priority bits.

v1 daemon implementation MAY treat EXPEDITE as a no-op and MAY not yet
implement shedding; the semantics are reserved and normative now so senders
can stamp classes immediately and behavior can turn on daemon-side later
without a wire change.

## 6. Decode-error taxonomy (additions)

- `UnsupportedVersion { ver }` — now the stale-binary tripwire (§2).
- `TooShortForHeader { have, need: 21 }` — need updated.
- `ReservedAdmissionClass { flags }` — admission bits `0b11`.
- `SheddableIllegalFrameType { ty, flags }` — class `10` on anything but
  `Push`/`StreamData`.
- `NonzeroEpochOnControlChannel { epoch }` — channel 0 with `epoch != 0`.
- Existing errors unchanged: `TooShortForPrefix`, `UnknownFrameType`,
  `ReservedFlagBits` (now bits 6-7 only), `ReservedPriorityBits`,
  `PureHeaderFrameWithBody`.

## 7. Surface changes by artifact

| Artifact | Change |
|---|---|
| `subc-protocol` | header codec 21B, `epoch` field, ver=2, flags field + decode errors, `RouteBind.epoch` |
| `subc-control` | `RouteOpen` response `route_epoch: u32` |
| `subc-core` | per-slot epoch counters (connection + endpoint scoped), assignment at reserve/commit (`commit_route`), validation+rewrite in relay path (`lookup_data_route` callers), stamped daemon-originated frames, allocator skip-live + retire-at-MAX |
| `subc-transport` | none beyond re-pin (framing reads len from frozen prefix; body offset via `HEADER_LEN`) |
| TS / Rust / Swift clients | envelope constants + codec, route cache stores `(channel, epoch)`, stamp on every frame, `routeOpen` epoch plumb, provider serve loop stamps module-side epoch; call options expose an optional per-call admission class (default NORMAL — first consumer: synapse EXPEDITE on interactive embed queries) |
| `subc-mcp` + all modules | rebuild against bumped crates; hand-rolled channel-0 matches unaffected (body shapes unchanged except RouteBind/RouteOpen additive fields) |
| Golden vectors | envelope/frame vectors regenerated for all three client languages; auth vectors unchanged (pre-envelope) |

`RouteTarget` (Tool/Management surface discriminator) is untouched — FED's
preservation ask; the fed forwarder needs no re-verification unless its
encoding changes (it does not).

## 8. Considered and declined (the negative space, on record)

- **Per-frame trust/principal** — bind-time stamping via RouteBind is
  correct; per-frame invites confused-deputy bugs. (AFT)
- **Tool-name / routing key in header** — body-opaque zero-deserialization
  stays. (AFT, unanimous)
- **Compression flag** — BINARY bit + body-level negotiation covers it. (AFT)
- **Transport-level sequence / stream-id / effect-id** — ordering is TCP +
  WAL-replay-on-resubscribe; exactly-once is durability-domain (intent logs,
  dedup ledgers); a header field cannot answer did-the-target-execute-before-
  dying and would be a second authority that can only agree or lie. The wire
  stays honest at at-most-once. (BROCA, ALF twin discharges)
- **Federation header space** — fed-frames are an independently-versioned
  Noise codec beside the loopback envelope; reserving subc-header space for
  fed would re-couple the two versioning timelines. Structural no. (FED)
- **Channel u32** — capacity fact + per-connection sharding hatch + flat
  dispatch; see §1. (Resolved against MC's initial lean; MC's correctness
  concern is carried entirely by the epoch.)

## 9. Flip-day cutover runbook

Mixed old/new-layout processes cannot parse each other; the cutover is
all-at-once, batched with a natural OpenCode restart window (ALF's ask).

1. Land the full change set on master (protocol + core + clients + vectors),
   CI green on all three OSes, cross-checked with the Windows clippy target.
2. Rebuild every fleet binary from the same protocol rev. The synchronized
   set is enumerated from the DAEMON'S LIVE ROSTER (`ck module list` +
   connected consumers), never from design-round attendance. As of this
   writing (9 supervised modules): `ck-subc`, `ck-subc-mcp`, `ck-aft`,
   `ck-mc`, `ck-thalamus` (live on every CC turn — a stale thalamus is a
   USER-FACING outage, first-class participant), `ck-broca`, `ck-quota`,
   `ck-credentials`, `ck-alfonso-core`, `ck-alfonso-routing` — plus the
   plugin-side consumers reloading at the OpenCode restart (alfonso TS
   plugin, AFT plugin, MC shadow lane — the latter fail-open, batched free)
   and the Swift client/chat app. Re-enumerate the roster at flip time.
   "Rebuild before next deploy" class (not live-supervised, off-cycle safe
   under the tripwire): fed module (dormant Hetzner box), `ck-synapse` (not
   in prod subc.jsonc), cortexkit-e2e suite (repin + rebuild before its next
   run). Peers rebuild their own repos (peer-owns-repo rule) against
   the bumped crates; publish `subc-protocol` + `subc-transport` in lockstep
   (republish-the-whole-chain rule) and `@cortexkit/subc-client` for the TS
   consumers.
3. Stage binaries at their deploy paths, signed (macOS codesign rule).
4. One window: stop daemon, swap binaries, `bootout`/`bootstrap` the launchd
   service, restart OpenCode so plugin-side clients reload. Two-artifact
   participants (alfonso: Rust modules + TS plugin; AFT likewise) must land
   both artifacts in the same window — a half-flip is loud-down under the
   ver tripwire (correct, but down until both sides match), never a silent
   desync.
5. Verify: fleet re-registers (`ck module list`), health table all-ok,
   `UnsupportedVersion` counter zero (any nonzero = a stale binary, named by
   connection), one end-to-end tool call + one broca turn + one MC transform.
6. Swift client + chat app rebuilt; golden-vector parity suites green in all
   three languages.

## 10. Test plan (gate-relevant)

- Golden vectors: 21-byte header encode/decode parity TS/Rust/Swift,
  including epoch boundaries (0, 1, u32::MAX) and all admission classes.
- Decode rejections: each new taxonomy entry, non-vacuous (asserts the exact
  error).
- Epoch lifecycle integration (subc-core): reuse-after-release delivers only
  current-epoch frames; stale-epoch frame on a reused slot is dropped and
  counted; stale GOODBYE does not tear down the new binding; reverse-lane
  Request with stale epoch is dropped (direction-agnostic proof); daemon
  rewrite correctness (client epoch ≠ module epoch on the same binding).
- Allocator: never assigns live slot; retires at u32::MAX (unit-level with an
  injected counter).
- Sheddable: daemon drop-under-pressure path drops SHEDDABLE Push/StreamData
  without closing the connection and without credit release/corruption;
  NORMAL frames keep today's escalation behavior (regression lock on the
  GOODBYE-escalation tests).
