# Scopes: owned identity records in the daemon

Status: design r4. Nothing here is built. r3 answered an Athena review of r2 (five seats, a
unanimous "do not implement as written"); r4 adds the room's review of r3. Section 10 lists
what changed and why. The
extensibility design (magic-context `.cortexkit/alfonso/plans/ck-extensibility-r6-7-amendments.md`,
section K2) relies on sections 2 to 6.

## 1. Why

Several modules act on behalf of a session they did not start: Broca runs a head that
Prefrontal owns, AFT carries that head's GitHub writes to Plexus, and Cerebellum holds grants
"for this conversation". Each needs to know, from something it can trust, which session a route
belongs to, who owns it, and which agent it acts for. A scope is one record the daemon holds and
stamps on the bind, in place of the self-asserted `session_ref`, `parent_session_ref`, runner
`owner` label and separate delegation registry.

It follows the admission-facts check (`control.rs:2937-2974`) and keeps both of its gates: only
configured principals may set the fields that grant authority, and the daemon stamps the record
without interpreting the rest.

Identity comes from the connection, never the request. `route_open_principal`
(`control.rs:2393`) makes an opener `reserved:<id>` only when its launch nonce matches a live
supervised launch, refuses `bad_consumer_identity` otherwise, and makes it `direct` when it
presents no identity. Every rule below uses that principal.

## 2. The record

A scope is identified by `(owner, ref)`:
- `owner`: the principal that registered it, from the registering connection. `direct` cannot
  own a scope.
- `ref`: an opaque string chosen by the owner, unique within that owner only, so no module can
  squat or block another owner's ref.
- `scope_epoch`: an unsigned integer the owner supplies and persists with its own record of
  the session. The owner re-sends the same value for the same session after any restart, its
  own or the daemon's, and uses a higher one when it reuses a ref for a new session. The daemon
  refuses a sync that lowers the epoch of a ref it holds (`scope_epoch_regressed`). Anything
  that must not carry over to a new session (a stored approval, a conversation grant, a frozen
  runner session, a background task) binds to `(owner, ref, scope_epoch)`. Because the owner
  keeps it, it survives daemon restarts and upgrades, which a daemon counter would not.
- `version`: a daemon counter increased on every change to the record within one incarnation.
  It is how a bind notices a change between admission and commit (section 4).

Fields:
- `kind`: a closed enum, `head | worker | ephemeral`.
- `parent`: optional `(owner, ref)` of another scope.
- `carriers`: the principals, other than the owner, that may open routes under the scope.
- `attributes`, settable only by the owner, anything else refused by name:
  - `agent_id` (string): the head's agent, on every head scope. It is identity, and the value
    today's `agentProjectId` admission fact already carries.
  - `delegates` (bool, default false): true lets a provider act as that agent. Refused without
    `agent_id`.
  - `hook_order` (closed enum, today only `exclusive`).

**Authority gate.** `agent_id`, `delegates` and `hook_order` may be set only by an owner listed
in the daemon config key `scope_authority_owners` (today `["prefrontal-core"]`, the same module
as `admission_facts_carrier_module_id`). A scope from any other owner may carry `kind`,
`parent` and `carriers` only; a sync that sets a gated attribute from an unlisted owner is
refused by name (`scope_attribute_not_permitted`). Providers still check the stamped `owner`
against their own expectation before honouring `delegates` (section 7).

Bounds: at most 10,000 live scopes per owner and 4 KiB of attributes per scope; past either the
sync is refused by name and nothing is applied. Tombstones (section 6) are capped at 1,000 per
owner and evicted oldest first; that bound never refuses.

## 3. Registering: `scope.sync`

An owner sends its full set: `scope.sync {generation, scopes: [...]}`. Once it has done that
on its current authority connection, it may send changes instead:
`scope.patch {generation, upsert: [...], remove: [...]}`. A patch changes only the named refs,
applies every per-record check a full sync applies, and follows the same generation rule. The
first sync after taking authority must be a full `scope.sync`; a patch before it is refused
`scope_patch_before_sync`. An owner with a large set (the Thalamus gateway owns every Claude Code
subagent scope on the machine) can then register one new scope without re-sending thousands.

**One sync authority per owner.** Each owner has exactly one connection whose syncs are
accepted: its authority. The first connection of an owner to sync becomes the authority. A
sync from any other connection of the same owner is refused `scope_sync_not_authority`. That
covers a blue/green swap, where the owner briefly has two connections: the candidate is refused
until cutover, and at cutover authority moves to the promoted connection in the same step as
the forwarding switch, without touching the set. The superseded connection's syncs are refused
from then on. When the authority connection closes, authority is free, and the next connection
of that owner to sync takes it.

**Generations.** A sync from the authority must carry a generation larger than the last one it
accepted; an equal or smaller one is refused as stale, and a refused sync changes nothing. The
first sync from a connection that has just taken authority is a full replace at any generation,
and its generation becomes the new baseline. So a restarted owner, or an owner after a daemon
restart, is never locked out, and a stale connection can never overwrite a newer one.

**Effect of a sync.**
- A scope present before and absent now is ended: tombstoned, and every route under it drained
  with reason `scope_ended`.
- A stamp is a snapshot taken at bind, so revoking authority ends the routes that carry the old
  stamp, each with its own reason so a carrier can tell them apart:
  - `scope_carrier_removed`: the opener is no longer a listed carrier;
  - `scope_delegation_changed`: `delegates` went from true to false;
  - `scope_ended`: the scope is gone, or replaced by a higher epoch.
  These are new `route.closed` reasons. Older SDKs map an unknown close reason to "do not
  reopen", which is right for the first and third; only carriers that use scopes, which are new
  code, receive any of them.
- A sync that gives a live `(owner, ref)` a higher `scope_epoch` ends the old scope (tombstone,
  drain with `scope_ended`) and creates the new one. A new session never inherits a live route,
  stamp or approval of the old one.
- `parent` is accepted only when the syncing owner is the parent's owner or a listed carrier of
  the parent, at the time of the sync. A cycle is refused.
- When a parent ends, its children stay and later stamps carry `parent_ended: true`;
  `scope.subscribe` reports the change (section 5). One owner's removal never tears down another
  owner's routes.
- `scope.sync` from `direct` is refused by name.

## 4. Opening a route under a scope

`route.open` gains `scope: {owner, ref, scope_epoch?}`. A carrier that froze an epoch (a runner's
session, a background task) always sends it, so a later lazy open can never be admitted against
a newer session that reused the ref. An owner opening for its current session may omit it. The
daemon admits the open only when the opener is the owner or a listed carrier, and otherwise
refuses by name:
- `scope_not_synced`: the owner has not synced since this incarnation started and is configured.
  Retryable: after a daemon restart a carrier's open can arrive before the owner re-syncs, and
  the carrier waits within its own bound.
- `scope_not_live`: the owner has synced and the ref is not in its set, or the owner is not a
  configured module. Terminal.
- `scope_ended`: the named `scope_epoch` does not match the live record. Terminal.
- `scope_not_carrier`: the opener is neither the owner nor a listed carrier. Terminal.

There is no relay class: a module that must present a scope onward is listed as a carrier. A carrier
route lives until the carrier closes it or the scope ends or changes as in section 3.

On admission the daemon captures `(scope_epoch, version)` into the pending bind and stamps the
bind with `scope {owner, ref, scope_epoch, kind, parent, parent_ended, attributes}`, next to the
principal it already stamps. The immediate opener stays in the principal field. The stamp also
carries `owner_authorized`, computed by the daemon: true when the owner is listed in
`scope_authority_owners`. Providers check that flag rather than keeping their own copy of the
list.

**Commit.** When the module acks the bind, the daemon checks the captured `(scope_epoch,
version)` against the current record before the route becomes routable. If the scope ended or
changed, the open is refused as `scope_ended` (terminal) or `scope_changed` (retryable: nothing
was sent, and the caller re-opens against the current record). That refusal is a settled
rejection, handled beside the existing superseded-endpoint arm in `complete_pending_relay`: it
releases the reserved route pair, sends the module a channel-scoped GOODBYE for the binding it
just created, and answers the waiting `route.open` with the named refusal. It is never an `Err`
from `commit_route_locked`, which would close the module's whole connection and every other
client's routes to it.

A provider must not take irreversible action inside `on_bind`: the route is not live until
commit, and commit can still refuse it.

## 5. Reading: `scope.describe` and `scope.subscribe`

`scope.describe {owner, ref}` answers:
- `status`: `live`, `ended` (tombstoned since this daemon incarnation) or `not_live`;
- `scope_epoch` for `live` and `ended`;
- `daemon_incarnation`;
- `owner_synced`: whether this owner has synced since this incarnation started;
- `owner_configured`: whether the owner is a module in the supervisor's roster;
- for `live`, the same fields as the stamp.

How a reader holding something bound to `(owner, ref, scope_epoch)` decides, in this order:
1. `live` with the same `scope_epoch`: use it.
2. `live` with a different `scope_epoch`: a new session under a reused ref. Refuse by name; the
   old binding is dead.
3. `ended`, or `not_live` with `owner_synced: true`: refuse by name. An evicted tombstone reads
   as `not_live`, which refuses too.
4. `not_live` with `owner_synced: false` and `owner_configured: true`: not re-synced yet. Hold,
   and refuse as `scope_unverifiable` past the reader's own bound.
5. `not_live` with `owner_configured: false`: the owner will never sync. Refuse the action.
Refusing an action never deletes what the reader stores; only an `ended` answer or event does.

`scope.subscribe` is a held request shaped like `supervisor.spawn_subscribe`, with the same
`{daemon_incarnation, seq}` cursor and too-old-cursor refusal. It sends a snapshot of live
scopes, then events:
- `{owner, ref, scope_epoch, ended}` when a scope ends;
- `{owner, ref, scope_epoch, changed}` when its carriers, `delegates` or `parent_ended` change;
- `{owner, synced}` when an owner's first sync of this incarnation is accepted.
After an owner's `synced` event, any scope of that owner the reader holds that is not in the
live set is ended. Before it, a scope merely missing is held, never treated as ended.

## 6. State, locking, restarts and upgrades

State, all in memory:
- in the scope table: records keyed `(owner, ref)`, per-owner authority connection, generation,
  `owner_synced` and the tombstones since this incarnation;
- in the forwarding table: each pending bind's and each route's scope tag `(owner, ref,
  scope_epoch, version)`, and the index from a scope to its routes.

Lock order is scope table, then forwarding table, always. Commit already holds the forwarding
write lock and never takes the scope lock: it compares the pending bind's captured tag with the
record's current `(scope_epoch, version)`, which a sync publishes into the forwarding table in the
same step that changes the record. Ending or changing a scope: take the scope write lock, update
the record, then take the forwarding write lock, publish the new `(scope_epoch, version)`, and
remove every route and pending bind tagged with the old one on every endpoint, including
superseded endpoints of a swap. A commit before that step is collected by it; a commit after it
sees the new version and refuses.

**Restarts and upgrades are the same case for scopes.** The table starts empty, every
`owner_synced` is false, `daemon_incarnation` is new, and readers hold as in section 5. The
in-place upgrade (`docs/designs/daemon-in-place-upgrade.md`) does not carry routes or pending
binds and gives the new process a new daemon id, so it does not hand over scope state: owners
re-sync and subscribers take a new snapshot, exactly as after a restart. `version` restarts, which
is safe because it is only ever compared within one incarnation. `scope_epoch` does not: the owner
re-sends it, so a stored approval survives a restart when the same session comes back, and dies
when a new session reuses the ref.

## 7. What providers must do

- Read identity only from the bind stamp, never from a request payload.
- Act as an agent only when `delegates` is true, `agent_id` matches, and `owner_authorized` is
  true. `agent_id` alone is identity, never
  permission to act.
- Bind stored approvals and grants to `(owner, ref, scope_epoch)`, and decide on them as in
  section 5.
- Treat the route's stamp as fixed for the route's life. A change that revokes authority drains
  the route (section 3), so a provider need not re-check per call.

## 8. Rollout

Readers first, then writers:
1. Providers that need a scope refuse a bind without a stamp. Today's daemon drops unknown
   `route.open` request fields, so a carrier naming a scope on an old daemon gets an unstamped
   route, which the provider's refusal makes fail closed.
2. The daemon ships `scope.sync`, `scope.describe`, `scope.subscribe`, the stamp and
   `scope_authority_owners`, advertised in `server.describe` as capability `scopes/v1`, and
   `scope_changed` and `scope_not_synced` join subc-protocol's retryable `route.open` set in the
   same release. An older
   SDK treats it as terminal, which is safe.
3. Owners and carriers use scopes only when the capability is advertised. A carrier that cannot
   open a scoped route fails the call (`scope_unsupported`) instead of opening an unscoped one.

## 9. Tests the daemon change must carry

Each fails by name when its rule is removed:
- only the owner or a listed carrier is admitted; `direct` can neither sync nor own;
- an open naming a `scope_epoch` other than the live one is refused `scope_ended`; an open before
  the owner's first sync of this incarnation is refused `scope_not_synced` (retryable), and after
  it, for a ref not in the set, `scope_not_live`;
- a patch changes only its named refs, is refused before a full sync, and obeys the same
  generation and per-record checks;
- the same ref under two owners is two scopes;
- a gated attribute from an owner not in `scope_authority_owners` is refused;
- a scope ended, or changed, between admission and commit refuses the open, the module's other
  routes stay up, and the reserved pair is released;
- removing a carrier and turning off `delegates` each drain the affected routes;
- ending a scope drains its routes on every endpoint, including a superseded one;
- a swap candidate's sync is refused, authority moves at cutover without changing the set, and
  the superseded connection's sync is refused afterwards;
- a restarted owner's first sync replaces at any generation, and an equal or smaller later one
  is refused without changing anything;
- a forged parent (neither owner nor carrier of the parent) is refused, and so is a cycle;
- a higher `scope_epoch` for a live ref ends the old scope first, a lower one is refused, and the
  same one re-synced after a daemon restart reads as the same session; `describe` separates the
  five reader cases;
- each revocation drains with its own reason; `owner_authorized` is true only for listed owners;
- `subscribe` emits `ended`, `changed` and `synced`, and resumes from a snapshot after a
  too-old cursor;
- the tombstone bound evicts and never refuses; the live-scope and attribute bounds refuse.

## 10. Changes from r2

From the Athena review of r2 (five seats):
1. The in-place upgrade does not carry routes or the incarnation. r2 said it did; it treated an
   upgrade as invisible. An upgrade is now handled exactly like a restart (section 6).
2. `agent_id`, `delegates` and `hook_order` are gated to `scope_authority_owners`. In r2 any
   owner could set them, so any supervised module could claim a user's agent.
3. The relay admission class is deleted. In r2 any module a session called could present its
   full stamp, `delegates` included, to any target.
4. A parent is accepted only from the parent's owner or carrier; r2's "serving it" clause let
   any callee forge a parent link.
5. The commit re-check is a settled rejection arm, not an `Err` that closes the module
   connection, and it reads a tag in the forwarding table instead of taking the scope lock.
6. Stamps are snapshots: removing a carrier or turning off `delegates` drains routes, and a
   change between admission and commit refuses the open.
7. One sync authority per owner, moved at cutover; r2's per-connection first sync let a swap
   candidate wipe the live set.
8. An owner-supplied `scope_epoch`, refused if it goes down, stops a reused ref carrying old
   approvals and, unlike a daemon counter, survives restarts; `subscribe` reports owner sync, so a reader knows when a missing
   scope means ended.
9. The tombstone bound evicts and never refuses; r2 read both ways.

From the room's review of r3:
10. `route.open` can name the `scope_epoch`, so a carrier's lazy open can't be admitted into a
    newer session under a reused ref.
11. `scope_not_synced` (retryable) is split from `scope_not_live` (terminal), so a carrier's opens
    during the post-restart re-sync window wait instead of failing.
12. `scope.patch` lets a large owner register or remove one scope without re-sending its set.
