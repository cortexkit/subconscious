# Activity ledger

Status: draft for review.

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
daemon fills in who:

- **Under a route.** The module cites `on: {route: {channel, epoch}}`, a route bound to
  the calling module. The daemon resolves the binding and stamps:
  - the route's opener principal;
  - its project root;
  - from the bound scope record, the scope owner, `agent_id`, `run_id` and `flow_id`.

  A route that isn't bound to the caller, or a stale epoch, refuses
  `activity_actor_unknown`.
- **Under a scope the caller owns.** The scope owner (prefrontal-core today) cites
  `on: {scope: {ref, epoch}}`. The daemon checks the caller is that scope's owner and
  stamps the scope's attributes. This is how core records events about an agent's
  work after the route that started it has closed.
- **No citation.** The actor is the module itself, for system actions such as a
  scheduled cleanup.

The daemon records its own events the same way, with itself as the module and the
caller's verified principal as the actor: a module started on a new build, stopped,
restarted, swapped, disabled, rescanned.

## A module records only its own kinds

An event kind is `<module_id>.<name>`. `name` is lowercase letters, digits and `_`,
at most 64 characters. The daemon refuses a kind whose prefix isn't the calling
module's id (`activity_kind_forbidden`). Manifests may later declare their kinds the
way they declare bus events; v1 doesn't require it.

## Wire

Additive channel-0 operations, advertised by the capability `activity-ledger/v1` in
`server.describe` and HELLO_ACK, so a module records only when the daemon supports it.

- **`activity.record`** (module connections only):
  - takes `{kind, summary, subject?, details?, on?}`;
  - `summary` is the human line a phone shows, at most 200 characters;
  - `subject` is a short, typed pointer, such as `{type: "repo", id: "cortexkit/aft"}`;
  - `details` is a JSON object of at most 4 KiB;
  - the reply carries the assigned `seq` and comes only after the append is written.
- **`activity.query`**:
  - takes `{agent?, project?, module?, kind?, since_ms?, until_ms?, limit, cursor?}`;
  - returns entries newest first, with a continuation cursor.
- **`activity.subscribe`**: takes `{since?}`, then streams entries as they are
  appended, with the same cursor, replay and lag rules as `supervisor.spawn_subscribe`.

Each stored entry has these fields:

- `seq`, `at_ms` and `daemon_incarnation`, all from the daemon;
- `module`, `kind`, `summary`, `subject`, `details`;
- `actor`, stamped as above: `principal`, `scope_owner`, `agent_id`, `run_id`, `flow_id`,
  `project_root`.

### Who may read

Reads expose what every agent has done, so they are operator surfaces only:

- direct connections (`ck activity`);
- `reserved:callosum`, which may forward only requests stamped from the operator's phone
  (the same `fed-phone:` gate Cingulate uses);
- `reserved:ckbus` for a mirror;
- later, `reserved:basal` for notice rules.

Agent sessions never reach channel 0, so no agent can read the ledger. Reads by other
modules are refused (`activity_read_forbidden`).

### What must never be recorded

Secret values. A credential use records the credential's id, account and target,
never the token. The daemon can't see the body's meaning, so this is a rule every
recording module follows and its tests pin. The daemon caps sizes and refuses a
`details` that isn't a JSON object.

## Storage

The same mechanism as the durable terminal history: append-only JSONL in the run
directory (`activity.jsonl`), written through the shared line sink. Rotation and
retention are both bounded: 30 days, or 64 MiB across generations, whichever comes
first. Both are daemon config keys under `activity`, and changing them needs no
restart.

Queries scan the retained generations. At the expected volume (consequential events,
a few thousand a day) that stays well under a second. An index waits until
measurements say otherwise.

A torn final line from a crash is skipped on read and the line boundary repaired on
reopen, as the terminal journal does today. Each module is capped at a burst of 60
events and 600 a minute (`activity_rate_limited`), so a buggy module cannot fill the
disk.

## Bus mirror

When the bus is installed, ck-bus subscribes with `activity.subscribe` and republishes
each entry on `ck.<account>.event.subc.activity.v1`. Flows and other consumers can
then react without polling. The ledger itself never depends on the bus.

## Later

- **Tamper evidence for team users.** Each entry gains `prev`, a hash of the previous
  entry, and the daemon periodically checkpoints the head hash to a place the user
  controls. The v1 entry format reserves the field name.
- **Notices from rules.** User notices become rules over the ledger, possibly flows
  that subscribe and write to the user's notice sink.

## Open questions

1. **The phone's read path.** Is a narrow Callosum forward of `activity.query` and
   `activity.subscribe` right, or should a module (Cingulate) serve the phone's view?
2. **Retention defaults.** 30 days and 64 MiB.
3. **Which events core records first.** The proposal is worker launched, settled,
   rejected and cancelled; flow installed, revoked and disabled; package approved and
   denied; grant given and revoked.
