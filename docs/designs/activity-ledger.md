# Activity ledger

Status: approved. Version 1 is built as described here; the sections below record the
details the build settled (cursors, limits, the rate algorithm, lifecycle triggers,
retention and mirror checkpointing).

## Purpose

One place that answers "what have my agents and modules been doing?", readable from
the phone and the desktop. It records consequential state changes, not every tool
call (transcripts already hold those). Examples:

- a flow installed, revoked or disabled;
- a worker launched, settled, rejected or cancelled;
- a permission or grant given or revoked;
- a package approved or denied;
- a merge or push;
- a credential used;
- a module started on a new build.

The ledger is for awareness. It is not an audit log with tamper evidence (see
"Later"), and it does not replace any module's own records.

## Why the daemon holds it

The daemon is the only component every installation has. The message bus and Engram
are optional, so a ledger built on either would be missing exactly where a user has
the least other visibility. The daemon also already knows the facts the ledger must
not take on trust: which module a connection belongs to (launch nonce), which route
a request arrived on, the scope that route was opened under, and its project root.

The ledger keeps the daemon's existing rule: the daemon does not read request bodies.
A module reports its own events through a dedicated channel-0 operation; the daemon
never inspects the module traffic to infer them.

## Who did it: daemon-stamped actors

An event's actor is never self-reported. A module recording an event says what
happened, and optionally which of its own routes or scopes it happened under. The
daemon fills in who. The request has no actor fields at all, so nothing the module
sends can set them; an unknown field refuses `activity_malformed`.

- **Under a route.** The module cites `on: {route: {channel, epoch}}`, a route bound to
  the calling module at that epoch. The daemon resolves the binding and stamps:
  - the route's opener principal;
  - its project root;
  - from the bound scope record, the scope owner, `agent_id`, `run_id` and `flow_id`.

  For a route opened under a scope, the scope's current record must still carry the
  tag the route was bound under. A route that isn't bound to the caller, a stale
  epoch, or a scope whose tag has changed since the route was bound refuses
  `activity_actor_unknown`.
- **Under a scope the caller owns.** The scope owner (prefrontal-core today) cites
  `on: {scope: {ref, epoch}}`. The daemon checks the caller is that scope's owner and
  that the scope passes the same liveness checks route admission uses: the owner is
  synced, the ref is live, the epoch is current and the deadline has not passed. It
  then stamps the scope owner and the scope's `agent_id`, `run_id` and `flow_id`. Any
  failed check refuses `activity_actor_unknown`. This is how core records events
  about an agent's work after the route that started it has closed.
- **No citation.** The actor is the module itself, for system actions such as a
  scheduled cleanup.

The daemon records its own module lifecycle events the same way, with itself (`subc`)
as the module and the requester's verified principal as the actor. See "Daemon
lifecycle events" below.

## A module records only its own kinds

An event kind is `<module_id>.<name>`, with `name` matching `[a-z0-9_]{1,64}`. The
daemon refuses a malformed kind, or one whose prefix isn't the calling module's id,
with `activity_kind_forbidden`. Manifests may later declare their kinds the way they
declare bus events; v1 doesn't require it.

## Daemon lifecycle events

The daemon records module lifecycle events under its own `subc.` prefix. Each has
subject `{type: "module", id: <module_id>}`, except a rescan, which names no single
module.

| kind | recorded when | actor principal |
|---|---|---|
| `subc.module_started` | a module registers and its build differs from the one in the most recent retained `subc.module_started` entry for that module, or no such entry exists; `details` carry the old and new build (`old_evidence`, `new_evidence`) | the requester if a control request caused the start, otherwise `subc` |
| `subc.module_stopped`, `subc.module_restarted`, `subc.module_swapped`, `subc.module_disabled` | the requested transition has taken effect | the requester |
| `subc.module_rescanned` | a rescan request changed the roster; `details` list the `added` and `removed` module ids | the requester |

- **A module's build** is its running-image digest: the executable identity the
  daemon already measures for `supervisor.provenance`, never a version the module
  declares. The comparison re-reads the retained entries, so a start whose previous
  `subc.module_started` entry has been pruned is recorded again.
- **An unmeasured build.** If the daemon could not measure the digest, it records
  nothing for that start and increments `activity_started_digest_skips` in
  `server.describe`.
- **The requester** is derived as supervisor requests already derive it: a registered
  module connection is that module's reserved principal, and anything else is Direct.
- **Refusals and no-ops record nothing.** Examples: disabling a module that is already
  disabled, or a rescan that changes nothing.
- **Crashes are not recorded.** v1 does not record crash restarts or exits that no
  request caused; the spawn feed and the terminal journal already hold them.
- **Supervision never waits.** Lifecycle appends are queued without waiting for
  storage and are exempt from the rate limit. There is no requester to refuse when one
  fails, so a failed or dropped lifecycle append is logged at warn with its kind and
  counted in `activity_lifecycle_append_failures` in `server.describe`.

## Wire

Additive channel-0 operations, advertised by the capability `activity-ledger/v1` in
`server.describe` and HELLO_ACK, so a module records only when the daemon supports it.

- **`activity.record`** (module connections only):
  - takes `{kind, summary, subject?, details?, on?}`;
  - `summary` is the human line a phone shows;
  - `subject` is a short, typed pointer, such as `{type: "repo", id: "cortexkit/aft"}`;
  - `details` is a JSON object;
  - the reply carries the assigned `seq` and is sent only after the append is written.
- **`activity.query`**:
  - takes `{agent?, project?, module?, kind?, since_ms?, until_ms?, limit, cursor?}`;
  - returns `{entries, cursor?, head}`, entries newest first.
- **`activity.subscribe`**: takes `{since?}`, then streams entries as they are
  appended. See "Cursors" for replay and lag.

An example record, from a module that settled a worker under a scope it owns:

```json
{"op": "activity.record",
 "kind": "prefrontal.worker_settled",
 "summary": "Worker settled: retry fix merged",
 "subject": {"type": "worker", "id": "wk_1842"},
 "details": {"outcome": "merged"},
 "on": {"scope": {"ref": "scope_7f3a", "epoch": 2}}}
```

Each stored entry has these fields:

- `seq`, `at_ms` and `daemon_incarnation`, all from the daemon;
- `module`, `kind`, `summary`, `subject`, `details`;
- `actor`, stamped as above: `principal`, `scope_owner`, `agent_id`, `run_id`, `flow_id`,
  `project_root`.

### Limits

- `summary`: 1 to 200 Unicode scalar values (not bytes, so a non-ASCII summary gets
  the same 200).
- `details`: a JSON object whose compact UTF-8 serialization is at most 4096 bytes.
- `subject`: exactly `{type, id}`. `type` matches `[a-z0-9_]{1,32}`; `id` is a string of
  1 to 256 scalar values.
- A wrong shape refuses `activity_malformed`. Examples: a `details` that isn't an
  object, an unknown or missing `subject` field, an empty `summary`.
- Exceeding a size bound refuses `activity_too_large`.

### Rate

Each module has a token bucket keyed by its module id. The bucket lasts for the daemon
incarnation, so a reconnect does not refill it.

- Capacity is 60 tokens, and the bucket starts full.
- It refills continuously at 10 tokens a second (one token every 100 ms, 600 a minute
  sustained), never above capacity.
- An accepted event spends one token. An empty bucket refuses `activity_rate_limited`.
- A refused attempt spends nothing, whatever refused it.
- Because the bucket starts full, up to 660 events can be accepted within one
  60-second window: the 60-token burst plus 600 refilled. This is intended.
- The daemon's own lifecycle events are exempt.

Disk use is bounded by retention's total size limit (64 MiB by default), whatever the
rate. The rate cap is about fairness: a buggy module recording in a loop can't push
every other module's entries out of that limit within minutes.

### Refusal codes

| code | meaning |
|---|---|
| `activity_kind_forbidden` | malformed kind, or a prefix other than the caller's module id |
| `activity_actor_unknown` | a cited route or scope failed a check |
| `activity_rate_limited` | the module's bucket is empty |
| `activity_read_forbidden` | the reader's principal may not read |
| `activity_too_large` | a size bound was exceeded |
| `activity_malformed` | a wrong shape |
| `activity_unavailable` | the sink can't open, a write failed, or the append queue is full |
| `activity_cursor_incarnation_mismatch` | a subscribe cursor names another daemon incarnation |
| `activity_cursor_too_old` | a subscribe cursor is older than the replay ring |
| `activity_subscriber_lagged` | a subscriber fell behind and its feed was ended |

The last three are the activity feed's counterparts of `supervisor.spawn_subscribe`'s
`spawn_cursor_incarnation_mismatch`, `spawn_cursor_too_old` and
`spawn_subscriber_lagged`, raised under the same conditions.

### Cursors

- **Ordering.** `seq` is ledger-wide and strictly increasing. An entry's stable
  identity is `(daemon_incarnation, seq)`. Ordering is always by `seq`, never by
  `at_ms`; the time filters select by `at_ms`, but entries with equal `at_ms` still
  come back in `seq` order.
- **Query pages.** The continuation `cursor` is the `seq` of the last entry returned,
  and the next page returns only entries with a lower `seq`. An append between pages
  therefore never shifts or repeats a page.
- **`head`.** Every query reply carries `head`, a subscribe cursor
  `{daemon_incarnation, seq}` naming the current incarnation and the highest `seq` at
  the moment the scan pinned its files. The scan pins its files under the writer lock,
  so no append can fall between the scan and `head`; filters and `limit` do not change
  `head`.
- **Subscribe.** `since` is a `{daemon_incarnation, seq}` cursor. The feed replays
  entries after it from the current incarnation's in-memory ring (the most recent 1024
  entries), then streams new appends. Without `since`, the feed starts at the next
  append.
- **A head from before a restart.** A cursor naming the current incarnation is valid
  even when its `seq` is lower than the first entry written in this incarnation. That
  is the usual case right after a restart, when `head` comes from retained entries.
  The feed then starts at the next append, and the cursor is not refused as
  `activity_cursor_too_old`.
- **Refusals.** A cursor from another incarnation refuses
  `activity_cursor_incarnation_mismatch`. Otherwise, a cursor older than the ring (the
  ring has already dropped the entry after it) refuses `activity_cursor_too_old`. A
  subscriber that falls behind (256 entries undelivered) is ended with
  `activity_subscriber_lagged`.
- **Recovery.** From any of these, page `activity.query` back to the last `seq` held,
  then subscribe from that reply's `head`. Every entry after `head` reaches the feed, so
  moving from query to follow leaves no gap and delivers nothing twice.
- **Delivery.** No entry reaches subscribers before its append has succeeded.

### Who may read

Reads expose what every agent has done, so they are operator surfaces only.

- On a registered module connection, the reader is that module's reserved principal,
  and any `consumer_identity` in the request is ignored, so a module cannot borrow
  another reader's identity.
- On a separate consumer connection, the reader's principal comes from the request's
  `consumer_identity`, verified against a supervised launch nonce exactly as
  `route.open` verifies it. A request without an identity is Direct. The SDK fills the
  identity from the launch environment, as it does for `route.open`, so supervised
  ck-bus and Callosum present their reserved principals.

Admitted readers:

- Direct (`ck activity`);
- `reserved:callosum`. Callosum forwards only requests from the operator's phone-class
  peers. That gate is Callosum's, and the daemon trusts the principal. Callosum is the
  phone's whole read path; no other module sits between them;
- `reserved:ckbus`, which reads the ledger to republish it on the bus (see "Bus
  mirror").

Every other principal refuses `activity_read_forbidden`. Agent sessions never reach
channel 0, so no agent can read the ledger.

A supervised module that opens a separate consumer connection without
`consumer_identity` is indistinguishable from the operator's `ck` and is admitted as
Direct. That is the existing local trust boundary every direct operation has. Closing
it belongs to operator elevation, separate later work to tell the operator's own direct
connections apart from any other local process; until then, every direct operation
shares this gap.

### What must never be recorded

Secret values. A credential use records the credential's id, account and target,
never the token. A module whose id is `deploy` records a credential use as:

```json
{"op": "activity.record",
 "kind": "deploy.credential_used",
 "summary": "Used the deploy credential for cortexkit/aft",
 "subject": {"type": "credential", "id": "cred_deploy_aft"},
 "details": {"account": "cortexkit", "target": "github.com"}}
```

The daemon can't interpret an entry's `details`, so it can't detect a secret there.
Keeping secret values out of `details` is a rule every recording module follows and its
tests pin. The daemon only caps sizes and refuses a `details` that isn't a JSON object.

## Storage

The same mechanism as the durable terminal history: append-only JSONL in the run
directory (`activity.jsonl`), written through the shared line sink (`cortexkit-log`).

- **Sequence.** On open, the daemon resumes one past the highest `seq` in the
  retained generations, or at 1 if none survive. `seq` therefore continues across
  restarts; `daemon_incarnation` changes.
- **Retention.** Bounded by age and by total size across generations, whichever is
  reached first. Defaults are 30 days and 64 MiB. Both are daemon config keys:

  ```json
  {"activity": {"max_age_days": 30, "max_total_mb": 64}}
  ```

  Both must be positive. A changed bound is applied at the next configuration rescan,
  without a restart: the writer reopens the sink with the new bounds while holding the
  writer lock, so no append or scan sees a half-applied change.
- **Size as file size × generations.** The sink's retention is per file: a file size,
  a count of rotated generations to keep, and an age. The total bound is therefore
  enforced as file size × generations, counting the active file. The file size is
  `ceil(max_total_mb / 256)` MiB (the sink keeps at most 255 rotated files), and the
  generation count is as many files of that size as fit in the total. The default
  64 MiB is 64 files of 1 MiB: the active file and 63 rotated generations.
- **Age.** A rotated generation whose last write is older than `max_age_days` is
  removed when the sink opens or rotates.
- **Torn lines.** A torn final line from a crash is skipped on read and the line
  boundary repaired on reopen, as the terminal journal does today. Readers count and
  skip any unparseable line; the count is `activity_unparseable_lines` in
  `server.describe`.

Queries scan the retained generations. At the expected volume (consequential events,
a few thousand a day) that stays well under a second. An index waits until
measurements say otherwise.

### Failure handling

The ledger never blocks or fails the daemon's routing, supervision or shutdown.

- Appends go through a bounded queue (64 entries) to one writer thread. Appends and
  scans run off the connection readers, and no lock is held across I/O on any
  connection's reader path.
- `activity.record` refuses `activity_unavailable` when the sink can't open, a write
  fails, or the queue is full. After a failed write the writer accepts no further
  appends until the sink is reopened, so a partial line is never joined to the next
  entry. Nothing panics.
- Shutdown never waits for the writer.

## Bus mirror

When the bus is installed, ck-bus subscribes with `activity.subscribe` and republishes
each entry on `ck.<account>.event.subc.activity.v1`. Flows and other consumers can
then react without polling. The ledger itself never depends on the bus; a bus outage
only delays the mirror.

- **Message.** Each message is the stored entry, which carries
  `(daemon_incarnation, seq)`. Consumers deduplicate on that pair.
- **Checkpoint.** The mirror's position lives in `activity_cursor.json`, beside
  `spawn_cursor.json` in ck-bus's state directory, written the same durable way (a
  sibling temporary file, then an atomic replace).
- **Advance only after publish.** The checkpoint advances only after the bus has
  stored the entry. This deliberately differs from the spawn consumer, which advances
  even when handling fails. When a publish or a checkpoint write fails, the mirror
  backs off and resumes from the saved checkpoint.
- **At-least-once delivery.** A crash between publish and checkpoint republishes that
  entry after restart: one duplicate with the same `(daemon_incarnation, seq)`, and
  nothing lost.
- **First run or damage.** An absent or damaged cursor starts from the current query
  `head`, with no backfill of history. A damaged cursor is logged.
- **Refused cursor or lag.** When `activity.subscribe` refuses the saved cursor
  (`activity_cursor_incarnation_mismatch` after a daemon restart, or
  `activity_cursor_too_old`) or ends the feed with `activity_subscriber_lagged`, the
  mirror fills the gap from `activity.query` pages between the checkpoint `seq` and
  `head`, publishes them in ascending `seq` order, checkpoints `head`, then subscribes
  from `head`. Entries already pruned from the ledger are logged as lost to the mirror
  (`ckbus.activity.entries_lost`).

## Reading from the desktop

`ck activity` takes the query filters (`--agent`, `--project`, `--module`, `--kind`,
`--since-ms`, `--until-ms`, `--limit`) and shows a table of time, module, actor and
summary. `ck --json activity` prints the entries as a JSON array (one entry per line
with `--follow`). `--follow` keeps reading the live feed with the same filters, moving
from the query to the feed through the reply's `head`, so nothing appended in between
is missed.

The Rust SDK (subc-client-rs) offers a module-side `record_activity` helper, which
checks the capability first and returns a typed refusal for every code above, and
consumer-side query and subscribe helpers.

## First events from prefrontal

All are recorded on the scope prefrontal owns, so each names its agent:

- a worker launched, settled, rejected, cancelled or failed;
- a campaign fired or closed;
- a flow installed (with its approval kind, card or policy, and the rule that allowed
  it), revoked or disabled;
- a package approved or denied;
- a grant given or revoked.

## Later

- **Tamper evidence for team users.** Each entry gains `prev`, a hash of the previous
  entry, and the daemon periodically checkpoints the head hash to a place the user
  controls. The v1 entry format reserves the field name.
- **Notices from rules.** User notices become rules over the ledger, possibly flows
  that subscribe and write to the user's notice sink. A notice service would read as
  `reserved:basal`, admitted alongside the readers above.
- **An agent reading entries about itself.** A lead agent may later want its own
  workers' entries. Because every entry already carries the daemon-stamped `agent_id`
  and scope owner, that is a read filter fixed to the caller's own agent, added
  without changing the entry format.

## Open questions

None. The one open question, retention defaults, is resolved: 30 days and 64 MiB,
both adjustable under the `activity` config keys.
