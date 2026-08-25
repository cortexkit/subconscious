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
One constant, zero machinery.

Two normative rules that make the tripwire actually fire (gate finding 6):

- **Prefix-first reads.** Every frame reader reads and validates the 5-byte
  frozen prefix BEFORE reading the remainder of the header. Normatively this
  lands in the SHARED `subc_transport::read_frame` (which the daemon, the
  Rust SDK, and hand-rolled Rust seams like fed's CatalogClient all use) plus
  each client library's own reader (TS `socket` reader, Swift). A reader that pulls a fixed 21 bytes up front
  would block forever on a stale sender's 17-byte pure-header frame (only 17
  bytes ever arrive) instead of erroring. Read 5, check `ver`, then read
  `HEADER_LEN - 5 + len`.
- **Exact-version handshake.** HELLO negotiation requires `protocol_ver == 2`
  exactly. The current `negotiate_version` clamps any offer above the
  minimum; that clamping is removed — single supported version means
  accept 2, reject everything else loudly.

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
  lazily-populated `slot → last_epoch` map.
- **Epochs are minted at RESERVATION time, not commit** (gate finding 5):
  when the daemon reserves the client/module channel pair for a
  `route.open`, it increments both slots' epoch counters and stores both
  epochs in the pending reservation (`PendingRouteBindRelay`). The epochs
  travel with the reservation into `commit_route` and into every abandonment
  path (abort GOODBYEs for failed binds are stamped with the reserved
  epoch). A reservation that is released without commit does NOT return the
  epoch — the increment is consumed either way, so a later rebinding of the
  slot is always distinguishable from the aborted reservation.
- `route.open` response (`ClientControlResponse::RouteOpen`) gains
  `route_epoch: u32` next to `route_channel`.
- `route.bind` (`ModuleControlRequest::RouteBind`) gains `epoch: u32` next to
  `route_channel` (the module-slot epoch).
- Both sides stamp their `(channel, epoch)` pair into every frame header on
  that route. Channel-0 frames carry `epoch = 0`.
- **Synchronous commit on bind-ack (closing the ack→commit window)**: the
  daemon commits the reservation into the live tables UNDER THE FORWARDING
  WRITE LOCK while processing the module's accepted RouteBind response —
  before the module connection can process any subsequent frame — rather
  than waking a waiter that commits later. The pending reservation carries
  the client sink/negotiated version and both epochs so the commit needs no
  second lookup (implementation note: extract a `commit_route_locked`
  operating on `&mut ForwardingInner`; the existing `commit_route` and
  `complete_pending_relay` already share the lock with no await points).
  Rationale: after queueing the RouteBind ack, SDKs invoke `on_bound`,
  which may synchronously emit an immediate reverse Request; if commit
  happened asynchronously, that legal post-ack frame could arrive
  pre-commit and be dropped or misjudged. With synchronous commit, any
  frame the module sends after its ack is ordered behind the commit on the
  same connection.
- **Client-side publication ordering (the other half of the window)**:
  committing before the module's next frame is not enough — the CLIENT must
  also learn the handle before any post-ack module traffic reaches it.
  Serialization is per-socket, so an immediate post-ack reverse Request
  relayed to the client could otherwise enter the client's FIFO AHEAD of the
  RouteOpen response, and §3.3 layer 2 would correctly drop it as an unknown
  slot — losing legal traffic. Normative recipe (the details are
  load-bearing): BEFORE relaying `route.bind`, the daemon acquires an OWNED
  client-egress permit (without holding the forwarding lock) and stores it
  in the pending reservation together with a PREBUILT RouteOpen response
  frame carrying the original client corr and negotiated version (the ack
  handler otherwise possesses only the module's response). The Accepted
  transition performs `commit_route_locked` and consumes that permit — table
  publication and response-queue insertion happen with NO unlock between
  them. Rejected/Aborted releases the permit exactly once. This ordering
  guarantee is PER ROUTE-HANDLE: frames for unrelated routes may legally
  appear before the RouteOpen response; only same-route traffic is ordered
  behind it. Consumer-SDK rule (normative, all three clients): the
  dispatcher installs the returned handle into its channel→epoch map BEFORE
  resolving the `routeOpen` caller and before reading the next frame off
  the socket.
- **Provider bind sequence (`on_bind` is decision-only)**: `on_bind`
  receives tentative bind metadata but MUST NOT emit route traffic — today's
  callbacks run before the ack is queued, and pre-ack traffic would reach a
  reservation and be dropped. After `on_bind` returns Accept, the provider
  SDK queues the RouteBind ack, installs the handle into its endpoint map,
  and only then invokes an optional `on_bound(RouteHandle)` callback, from
  which immediate route traffic (e.g. a reverse Request) may legally begin.
  On rejection or ack-queue failure, the handle is never installed and
  `on_bound` is not invoked; the SDK directly runs the handler-state
  cleanup routine with the tentative full handle, without relying on an
  ingress GOODBYE. For HAND-ROLLED serve loops (no callback seam to
  rename) the same rule is stated as a testable invariant: no route-scoped
  egress frame for a binding may precede that binding's RouteBind ack in
  the writer queue. Handler-side convention (one convention, not per-handler
  choice): handler-owned session state MAY be installed at `on_bind`
  (decision time) — the SDK's serialized dispatch guarantees no request
  reaches the handler before the ack, so decision-time install is safe and
  cheap; only route-scoped EGRESS waits for `on_bound`.
- **Single-winner bind resolution**: `Pending → Committed | Aborted |
  Rejected` is ONE write-locked transition. The arbitrating participants are
  DAEMON-SIDE ONLY: module reply, daemon relay deadline, owner teardown
  (drain), and connection death. Exactly one wins; the winner alone performs
  commit/release/abort-GOODBYE, every loser observes the terminal result and
  performs NOTHING. SDK-LOCAL timeout/cancel is NOT an arbitration
  participant — it alters no daemon state (there is no concurrent channel-0
  cancel path; the client's socket loop is serial). The SDK rule for a
  locally-timed-out `route.open` (normative, all three clients): retain the
  control-op identity; if a successful RouteOpen response later arrives, do
  NOT silently drop it — the daemon has committed a live route — immediately
  close it with a GOODBYE for the returned handle (escalating to connection
  close if that GOODBYE cannot be queued), so a late-committed route is
  never orphaned. The same single-winner arbitration applies to
  module-control RPCs (whose completion path today ignores the stored
  deadline).

### 3.2.1 Client API shape: the route handle (gate finding 4)

Wire route identity is `(channel, epoch)`. Each SDK wraps it in an
immutable `RouteHandle` that ADDITIONALLY carries an opaque, non-wire
CONNECTION TOKEN minted uniquely for the live socket — epochs are scoped to
a connection, so a handle from connection C1 must never act on C2, where
the same `(channel, epoch)` pair can legitimately identify a different
route after reconnect. Every route-scoped operation (request, subscribe,
cancel, close, poll) takes the handle, never a bare channel, and requires
token identity to match the SDK's current connection — otherwise it fails
locally WITHOUT emitting any frame. The token changes on reconnect and is
retained by callbacks, pending state, and reverse replies; only `channel`
and `epoch` are serialized. Precisely: `RouteHandle` is an immutable SDK
object carrying `channel`, `epoch`, and the opaque connection token; the
token may be implemented as private state or object identity, is checked
only by the SDK, and is never serialized or interpreted by the daemon (the
daemon needs no token awareness — connection identity already separates its
spaces). `on_bind` receives the tentative full handle; `on_bound`,
route-cleanup callbacks, pending/provider state, and reverse-reply contexts
all retain that full handle. Managed route caches store handles; the close-beats-reopen
generation guards compare full handles. This is load-bearing, not
ergonomics: an API that accepts a bare channel and looks up "the current
epoch" at send time would stamp STALE application work with the NEW epoch
after a reuse, laundering exactly the frames the epoch exists to kill. The
epoch a request carries must be the epoch its route was opened under.
Internal callers holding bare channels today (`subc-mcp` binding table,
fake-aft-stub, `ck`/`subc-probe`, Swift client) migrate to handles.

### 3.3 Validation: two layers, both mandatory

**Layer 1 — daemon relay** (`lookup_data_route` ingress, both directions):

- Binding found for `(connection|endpoint, channel)` **and**
  `frame.epoch == binding.<side>_epoch` → forward (rewriting channel+epoch
  to the binding's OTHER-side values).
- Binding found, epoch mismatch → **DROP** the frame (debug log + counter,
  never an Error frame — erroring would inject into the *new* binding's corr
  space, which is the confusion the epoch exists to prevent).
- No binding → unchanged existing behavior (`unknown_channel` Error for
  Requests, silent drop otherwise), with the Error frame stamping the
  INGRESS frame's validated epoch (daemon-synthesized route Errors always
  copy the epoch of the frame they answer; a mismatched frame produces no
  Error at all).

**Layer 2 — receiving endpoints** (gate finding 1, BLOCKER-driven): relay
validation alone is insufficient because forwarding is not atomic with the
table lookup — the daemon snapshots the binding, releases the table lock,
then enqueues; a release+rebind can interleave, so a frame validated against
epoch E1 can arrive at an endpoint after its slot was rebound to E2.
Therefore every endpoint (all three clients' consumer AND provider/serve
loops, module serve loops via the SDKs) maintains its own `channel → epoch`
map for live routes and validates every nonzero-channel ingress frame
BEFORE dispatch or any lifecycle effect: epoch mismatch or unknown slot →
silent drop (counter). This OVERRIDES ordinary endpoint unknown-channel
behavior: Request, Cancel, and Goodbye alike are dropped without an Error
frame or lifecycle callback for uninstalled/mismatched handles — only the
daemon's layer 1 emits `unknown_channel` (today's provider loops dispatch
any nonzero Request and apply any Goodbye without a route map; that
behavior ends). Provider-side handle liveness: the metadata `on_bind` sees
is TENTATIVE — the handle becomes live in the endpoint map only once the
accepted RouteBind ack is queued, and route traffic legally begins at
`on_bound` (§3.2); a rejected bind never installs it. In-flight request state is keyed by `(channel, epoch, corr)`,
so a stale frame can never settle a new binding's request even if corr
values collide across generations. Because the daemon rewrites the epoch to
the receiving side's slot epoch at relay, an endpoint always compares
against its own locally-known epoch — the two-spaces model holds.

**Epoch-fenced release**: FRAME-DRIVEN, route-scoped release (GOODBYE
handling, `release_client_route`, `release_module_route`) is a single
write-locked compare-and-remove taking `expected_epoch`; it removes the
binding ONLY if the live epoch matches, returning distinct
stale/absent/removed outcomes. A GOODBYE validated against E1 can therefore
never tear down an E2 binding installed between its validation and its
release — the TOCTOU is closed under the lock, not by ordering.
OWNER-SCOPED teardown is a different shape and has two variants, no
caller-supplied epoch in either — the owner identity plus a persistent mark
is the fence: `cleanup_connection` marks the owner closing and removes its
bindings and reservations in ONE locked transition. Endpoint DRAIN is
deliberately two-phase (matching the implemented drain-to-quiescence):
phase one marks the endpoint draining, closes Request admission on every
current live binding (so the in-flight count can only fall), and aborts its
reservations in ONE locked transition; after quiescence, phase two removes
the endpoint's then-current live bindings in one later locked batch. The persistent
draining mark rejects allocation and commit BETWEEN the phases — nothing
can rebind into a draining endpoint, which is what makes the gap safe. Peer-notification GOODBYEs are emitted only from
actually-removed bindings, stamped with each binding's epochs.

**Reserved-slot ingress**: a reservation is NOT a data route. Until commit,
a client Request on a reserved slot gets the existing `unknown_channel`
Error (stamped with the ingress epoch); every other frame — either
direction — is dropped. Module traffic cannot legally exist pre-commit
(§3.2's ordering), so a module frame on a reserved slot is always stale or
misbehaving: drop.

**Epoch-fenced escalation (delivery-failure close)**: the same fence
governs the OTHER destructive reaction to a route frame — connection-close
escalation on failed client enqueue. Today a full client egress queue
closes the client connection; if the failing frame was snapshotted under
epoch E1 and the slot (or a fresh reservation) has since moved to E2,
closing the connection would destroy E2 state from outside the fence.
Normative: escalation is a write-locked
`escalate_client_delivery_failure(connection, channel, expected_epoch)`
whose predicate is keyed on PUBLICATION, not reservation: the daemon
maintains `last_published_epoch` per client slot, advanced ONLY by the
locked commit-and-RouteOpen-enqueue transition (§3.2). Escalation closes
iff `last_published_epoch == expected_epoch` — regardless of whether the
failing frame came from a live or an already-removed binding (peer GOODBYEs
are emitted after removal, so a failed GOODBYE's binding is gone by
construction). An uncommitted or ABORTED successor reservation does NOT
invalidate escalation (the endpoint still knows only the failing
generation, and the close-on-failure reliability floor must hold); a
COMMITTED/PUBLISHED successor does invalidate it (the endpoint has moved
on; closing would destroy the successor from outside the fence). Keying on
reservation-advanced `last_epoch` instead would create escalation
false-negatives via aborted reservations — a reliability regression.

When escalation does proceed, the connection is marked closing inside the
forwarding table (under the same lock) BEFORE the out-of-band close
registry is signaled (the two are separate lock domains; mark-then-signal
is the required order), and allocation/commit reject marked connections —
so no new reservation can race into a connection that is being torn down.
Escalation call sites retain the failing frame's epoch (carried on
`RouteBinding`/`GoodbyeTarget`/`RouterError`).

Direction-agnostic is load-bearing (AFT's pin, ALF concurring): reverse-lane
Requests (module→client elicitation/sampling, `execute_effect`) are validated
identically at both layers — a consent prompt surviving a rebind onto a new
epoch is a cross-tenant delivery. Pure-header frames (`Cancel`, `Goodbye`,
`Ping`, `Pong`) carry and are validated on epoch like any other frame.
Daemon-originated frames (cleanup GOODBYEs, module-death propagation, drain
GOODBYEs, abort-bind GOODBYEs) are all stamped with the target binding's (or
reservation's) epoch — the carrying types (`GoodbyeTarget`, `RouterError`
paths, `PendingRouteBindRelay`) gain epoch fields so no synthesized frame
ever goes out unstamped.

### 3.3.0 RouteBind on an installed channel: implicit-replace on higher epoch

The daemon never re-issues `RouteBind` on a LIVE channel — the allocator
skips channels present in the live map or the pending-reservation map, so a
slot is reallocated only after its binding is released. But a provider
endpoint can still legitimately receive a `RouteBind` for a channel it
believes installed, because the daemon→module route-gone GOODBYE is
best-effort (dropped on module egress backpressure by the co-tenant
protection policy): the client releases the route, the daemon frees the
slot without the module learning, and a later `route.open` reuses the
channel with a freshly minted epoch. Per-slot epochs are persistent and
strictly monotonic (allocation mints `last_slot_epoch + 1`; a slot at
`u32::MAX` is retired, never reused), so the daemon never repeats or lowers
an epoch for one `(endpoint, channel)`.

Normative endpoint rule (all provider/serve loops, SDK-carried and
hand-rolled alike): a `RouteBind` whose channel is currently installed is
handled by epoch comparison —

- **strictly higher epoch → implicit replace**: the stale install is
  unreachable by construction (the daemon freed that binding); tear it down
  locally (no GOODBYE emission for it) and process the bind normally.
- **equal or lower epoch → reject** the bind (protocol violation; the
  daemon never does this).

Rejecting all binds on installed channels is a defect: one dropped
route-gone GOODBYE would wedge the slot forever on that endpoint.

### 3.3.1 Route-referencing channel-0 operations (gate finding 3)

Any channel-0 payload that references a route by channel number must carry
the epoch alongside, and the daemon validates the pair atomically against
the live binding (or reservation) under the table lock:

- `ModuleControlPush::RouteStatus` gains `route_epoch: u32`; a status push
  whose epoch does not match the current binding/reservation of that module
  slot is dropped, never cached — a delayed status from a dead route can no
  longer poison the status cache of the slot's new tenant.
- `ClientControlRequest::RoutePoll` gains `route_epoch: u32`; a poll carrying
  a stale epoch answers "unknown route" rather than reporting the new
  binding's state to a holder of the old one. The RESPONSE
  (`ClientControlResponse::RoutePoll`) echoes `route_channel` and
  `route_epoch` in every arm (including unknown-route), and the polling
  endpoint matches the echoed handle against its expected handle before
  settling or caching — channel-0 responses ride `epoch=0` envelopes, so
  without the echo a delayed poll response from generation E1 could settle
  an E2 poll on a reused corr. General echo rule: any control RESPONSE
  answering a route-scoped query names the same `(channel, epoch)` it
  answered for.
- Rule for the future: ANY new control op that names a route names it as
  `(channel, epoch)`. A bare channel number in a control body is a spec
  violation.
- **Poll linearization**: the daemon answers `route.poll` from ONE locked
  snapshot that performs the epoch validation AND reads the reported state
  (binding, module identity, cached status) in the same critical section —
  never validate-then-lookup as separate steps, which would let E1 validate,
  E2 rebind, and E2's state be reported under E1's echoed handle. The status
  cache is keyed by the full client handle `(channel, epoch)` (or
  equivalently cleared exactly-at-generation on release), so a stale entry
  can never answer for a successor. Absent/stale results are explicit:
  status query → `status: null, live: null`; liveness query →
  `status: null, live: false`; both always echo the REQUESTED handle.

**Channel-0 correlation hygiene** (the control channel has no epoch, so
corr is its only generation axis): each REQUEST-ORIGINATING endpoint has
exactly ONE monotonic allocator for all channel-0 requests it originates —
the daemon's bind relays and module-control RPCs share one allocator per
target connection/endpoint (today these are two independent wrapping
cursors, which lets a late route-bind rejection settle a health RPC that
reused its corr); a client/module likewise mints all its outbound control
corrs from one monotonic counter (today: subc-client-rs saturates at
u64::MAX, subc-mcp and the module-control helper wrap — all become
monotonic-no-reuse). The two DIRECTIONS are independent spaces: daemon-
originated and peer-originated requests may use the same corr value
simultaneously without collision, because responses correlate within the
originator's space. Responses, Errors, Pongs, and acks ECHO corrs, never
mint; uncorrelated Push/Goodbye use corr 0. Corr values are never reused
within a connection's lifetime; on exhaustion, close and re-establish the
connection (unreachable in practice — the rule closes the analysis). Late
responses to already-settled corrs are dropped.

Golden JSON vectors for the changed payloads are regenerated.

### 3.4 Allocator interaction and wrap

- The channel allocator never assigns a slot with a live binding (existing
  behavior, now stated as an invariant); released slots are freely reusable —
  the epoch disambiguates generations.
- A slot whose epoch reaches `u32::MAX` is **retired**: `MAX` is assigned to
  at most one final reservation, after which the slot is ineligible for
  every subsequent reservation for the lifetime of its connection/endpoint
  (allocator skips it). Wrap is thereby impossible by construction, not
  merely improbable. (4B rebindings of one slot within one connection
  lifetime is unreachable; the rule exists so the analysis is closed, not
  statistical.)
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
| 6    | DAEMON_ORIGIN   | daemon-authored envelope; forwarded frames clear it |
| 7    | reserved        | must be zero, decode-rejected; stale-binary tripwire |

Admission class (one 2-bit field, not two independent bits — per-frame the
states are exclusive and `EXPEDITE+SHEDDABLE` dies by construction):

`DAEMON_ORIGIN` is accepted by all Phase 1 decoders but is not set by ordinary
emitters. In the Phase 2 routing order, forwarded frames clear bit 6 and
daemon-authored errors set it; retry and classification guidance deliberately
waits for that phase.

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

Flip-day implementation posture (resolving the optional-vs-tested ambiguity):
the daemon MAY treat EXPEDITE as a relay no-op and MAY NOT yet implement
shedding — the semantics are reserved and normative now so senders can stamp
classes immediately and daemon-side behavior can turn on later without a
wire change. What IS mandatory at flip-day and tested (§10): decode
acceptance/rejection of all class values, end-to-end class-bit preservation
through relay, and — whenever shedding is implemented — the no-credit-effect
and no-connection-close properties. The drop-under-pressure behavioral test
ships with the shedding implementation, not with flip-day.

## 6. Decode-error taxonomy (additions)

- `UnsupportedVersion { ver }` — now the stale-binary tripwire (§2).
- `TooShortForHeader { have, need: 21 }` — need updated.
- `ReservedAdmissionClass { flags }` — admission bits `0b11`.
- `SheddableIllegalFrameType { ty, flags }` — class `10` on anything but
  `Push`/`StreamData`.
- `NonzeroEpochOnControlChannel { epoch }` — channel 0 with `epoch != 0`.
- Existing errors unchanged: `TooShortForPrefix`, `UnknownFrameType`,
  `ReservedFlagBits` (bit 7 only), `ReservedPriorityBits`,
  `PureHeaderFrameWithBody`.

## 7. Surface changes by artifact

| Artifact | Change |
|---|---|
| `subc-protocol` | header codec 21B, `epoch` field, ver=2, flags field + decode errors, `RouteBind.epoch`, `RouteStatus.route_epoch` |
| `subc-control` | `RouteOpen` response `route_epoch: u32`; `RoutePoll` request `route_epoch: u32` + response handle echo in every arm |
| `subc-core` | per-slot epoch counters (connection + endpoint scoped), minting at reservation (`PendingRouteBindRelay` carries both epochs + client sink/version), synchronous commit-on-bind-ack under the write lock, epoch-fenced compare-and-remove release paths, epoch-fenced delivery-failure escalation + connection closing-mark, validation+rewrite in relay path, epoch fields on `GoodbyeTarget`/route-Error synthesis, `RouteStatus`/`RoutePoll` epoch validation + handle echo, allocator skip-live + retire-at-MAX, exact-version handshake |
| `subc-transport` | prefix-first `read_frame` (`frame_io.rs` is the shared fixed-header reader: read+validate the 5-byte prefix, then the remaining header, preserving the body-size rejection before allocating `len` bytes) |
| TS / Rust / Swift clients | envelope constants + codec; `RouteHandle {channel, epoch}` as the only route-scoped API currency (§3.2.1); endpoint-side ingress epoch validation + `(channel, epoch, corr)` in-flight keying (§3.3 layer 2); `routeOpen` epoch plumb; provider serve loop stamps + validates module-side epoch; call options expose an optional per-call admission class (default NORMAL — first consumer: synapse EXPEDITE on interactive embed queries) |
| `subc-mcp` + all modules | rebuild against bumped crates; ALL bare-channel state in subc-mcp migrates to `RouteHandle`/`(channel, epoch, corr)`: `SessionInner.routes`/`ToolBinding`, `PendingKey (u16,u64) → (u16,u32,u64)`, `ReverseRelay.routes`/`pending`; `subc_reader_loop` performs endpoint validation before any dispatch; reverse replies retain the INGRESS handle (never look up the current epoch at reply time); fake-aft-stub, `ck`, `subc-probe` likewise; hand-rolled channel-0 matches: RouteBind/RouteOpen/RouteStatus/RoutePoll gain epoch fields (additive), golden JSONs regenerated |
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
   connected consumers), never from design-round attendance. Hand-rolled
   route-plane seams (not SDK-carried, self-identified via source audit:
   AFT's module frame loop, broca's two seams, subc-mcp) land the
   endpoint-validation layer + `(channel, epoch)` state migration in their
   own code — EXCEPT MC's TS shadow-sender (hand-rolled today), which
   instead DELETES its private framing and migrates onto
   `@cortexkit/subc-client` at the flip, inheriting the validation layer
   (hardening a temporary fail-open lane privately would be waste). As of
   this writing (9 supervised modules): `ck-subc`,
   `ck-subc-mcp`, `ck-aft` (hand-rolled serve loop + TS plugin transport +
   test drivers — three seams),
   `ck-mc`, `ck-thalamus` (live on every CC turn — a stale thalamus is a
   USER-FACING outage, first-class participant), `ck-broca` (BOTH seams
   hand-rolled, not SDK-carried: broca-subc consumer + broca-module-serve
   loop each land the endpoint-validation layer and RouteHandle migration in
   broca-owned code; broca-session/broca-run/ck-import rebuild with it;
   same user-facing class as thalamus), `ck-quota` (hand-rolled codec:
   direct subc-protocol/transport deps, own frame loop — no SDK seam; epoch
   threading is module-owned, verified via its real-daemon round-trip),
   `ck-credentials` (hand-rolled codec: module + CLI admin client + probe, direct protocol/transport deps — layer-2 validation self-implemented), `ck-alfonso-core`, `ck-alfonso-routing` — plus the
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
3. Pre-staging grep for every module that constructs `ModuleManifest` by
   hand: no `protocol_ver` LITERAL — declare `subc_protocol::PROTOCOL_VERSION`
   instead. A stale hardcode is rejected at registration (loud, by design)
   but surfaces one layer up as "module never appears in the catalog," which
   costs a log dive to localize (FED hit this during staging). SDK-carried
   HELLO is immune; the manifest field is author-supplied.
4. Stage binaries at their deploy paths, signed (macOS codesign rule). Owners
   with versioned staging rituals (AFT's `aft-stable`/`ck-aft` symlink +
   NDJSON versioned cache) restage in the same window so a post-flip module
   respawn cannot resolve a ver-1 binary.
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
  including epoch boundaries (0, 1, u32::MAX). Parity covers NORMAL,
  EXPEDITE, and SHEDDABLE on legal frame types; malformed vectors cover
  class `11` and SHEDDABLE on every illegal frame type (decode-reject, not
  round-trip).
- Decode rejections: each new taxonomy entry, non-vacuous (asserts the exact
  error).
- Epoch lifecycle integration (subc-core): reuse-after-release delivers only
  current-epoch frames; stale-epoch frame on a reused slot is dropped and
  counted; stale GOODBYE does not tear down the new binding (epoch-fenced
  release proof: interleave release/rebind between lookup and release, assert
  E2 survives); reverse-lane Request with stale epoch is dropped
  (direction-agnostic proof); daemon rewrite correctness (client epoch ≠
  module epoch on the same binding); abandoned-bind abort GOODBYE carries the
  reserved epoch; stale RouteStatus push does not poison the status cache of
  a reused slot; stale RoutePoll answers unknown-route; delayed E1 RoutePoll
  response on a reused corr does not settle an E2 poll (handle-echo proof);
  poll snapshot linearization (E1-validate/E2-rebind interleave reports
  under the correct handle, never E2 state under E1's echo); E1-snapshot →
  E2-rebind → full-sink escalation does NOT close the connection
  (epoch-fenced escalation proof, data frame and GOODBYE variants; plus the
  removed-binding arm: no-reuse ⇒ close, E2-reservation/rebind ⇒ no close);
  immediate post-ack reverse Request from a module is delivered to the
  consumer AFTER the RouteOpen response installs the handle (end-to-end
  through the actual consumer endpoint, not just daemon-side commit
  ordering); bind single-winner arbitration (timeout-fires-after-ack-commit
  observes Committed and aborts nothing; both lock-order interleavings);
  late cross-class channel-0 Error after timeout settles nothing (shared
  monotonic corr allocator proof); escalation publication-fence arms:
  removed-binding arms: no successor or only an uncommitted/ABORTED E2
  successor reservation ⇒ close (last_published_epoch unmoved),
  committed-and-published E2 successor ⇒ suppress; egress permit released exactly once on Rejected/Aborted (and
  receiver-closure path); client-cleanup-vs-Accepted exercises both lock
  winners (cleanup-first marks closing and aborts before commit;
  Accepted-first atomically commits-and-enqueues, then cleanup removes the
  binding and notifies the module — commit and enqueue are one locked
  transition, no interleaving exists between them); SDK late-RouteOpen
  cleanup (locally-timed-out open receiving a late success closes the
  returned handle with GOODBYE, never orphans; if SDK egress is
  full/closed and the GOODBYE cannot queue, the SDK closes the connection
  and daemon owner-cleanup removes the committed route); endpoint
  unknown-slot precedence (uninstalled-handle Request/Cancel/Goodbye
  dropped silently, no Error, no callback); connection-token fencing (C2
  reopening the same (channel, epoch) wire pair: C1's stale request, poll,
  cancel, and GOODBYE emit NO frame — fail locally on token mismatch);
  provider bind sequence (on_bind emits nothing; traffic begins at
  on_bound after ack queue + handle install); channel-0 allocator
  exhaustion (injected u64::MAX: each allocator emits MAX at most once
  then closes/reconnects, never emits 0, never reuses);
  connection-token fencing extended to delayed reverse replies and stale
  work captured by on_bound closures (local failure, no frame emitted);
  aborted-reservation reuse forcing the same client and module slots
  proves BOTH epochs advance; reserved-slot ingress proves client Request
  → epoch-stamped unknown_channel and every other client/module frame →
  silent drop; module-control deadline/reply arbitration exercises both
  lock winners, including an after-deadline reply arriving before the
  timer wakes; drain phase-gap closure (admission, reservation, and commit
  all remain closed between drain phases; no rebind enters before phase
  two).
- Endpoint-layer validation (all three clients): a frame carrying a stale
  epoch delivered PAST the daemon (harness-injected) is dropped by the client
  without settling in-flight state or firing handlers; in-flight keyed by
  (channel, epoch, corr) proof: same corr across two epochs settles only its
  own generation.
- Stale-binary tripwire: a v1 (17-byte) pure-header frame against the new
  reader produces UnsupportedVersion{ver:1} promptly (prefix-first read
  proof, non-blocking); HELLO offering protocol_ver=1 (or 3) is rejected
  loudly — no clamping.
- subc-mcp endpoint: an E1 host reply arriving after E2 slot reuse is dropped
  by the retained-ingress-handle rule, never delivered to the E2 session.
- Allocator: never assigns live slot; assigns u32::MAX at most once, then the
  slot is ineligible for every subsequent reservation (unit-level with an
  injected counter).
- Admission class at flip-day: decode acceptance/rejection of all class
  values; end-to-end class-bit preservation through relay; NORMAL frames keep
  today's escalation behavior (regression lock on the GOODBYE-escalation
  tests). The SHEDDABLE drop-under-pressure/no-close/no-credit behavioral
  test ships WITH the shedding implementation (conditional — not a flip-day
  gate), consistent with §5's flip-day posture.
