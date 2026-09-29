# Scopes: owned identity records in the daemon

Status: design r7. Nothing here is built. r3 answered an Athena review of r2 (five seats, a
unanimous "do not implement as written"); r4 added the room's review of r3, r5 added targeted
carriers, r6 answered the Athena review of extensibility r7, and r7 answers the Athena review of
extensibility r7.2 (rows T2-T22, T85, T87 of its triage table). `scope.subscribe` and
`scope.patch` are specified here but deferred to stage 7; until then providers read `describe`. Section 10 lists
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

**What that does and does not protect against.** The launch nonce is a bearer value, and any
connection presenting it is admitted as `reserved:<id>`. Today it is delivered in the module's
environment, which a same-user process can read (`ps eww`), so today scopes protect against bugs
and accidents, not against local code running as the user, a prompt-injected agent's shell
included. `docs/designs/launch-nonce-descriptor.md` is the stage-2 track that closes it: the nonce
over an inherited pipe, and every module and ck-subc signed with hardened runtime so no same-user
process can attach and read it. It makes no boundary claim until its last step, which ships in
stage 7.

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
- `version`: a daemon counter increased when a record's content changes, within one incarnation.
  A sync that re-sends a record unchanged does not move it.
  It is how a bind notices a change between admission and commit (section 4).

Fields:
- `kind`: a closed enum, `head | worker | ephemeral`, fixed for the life of an epoch. A sync that
  changes it at the same epoch refuses that record `scope_kind_changed`; the owner uses a higher
  epoch, which ends the old scope.
- `parent`: optional `(owner, ref, scope_epoch)` of another scope. The epoch pins the parent
  session. The stamp reports the link as `parent_state`: `linked` (the parent is live at that
  epoch), `pending` (the parent's owner has not synced in this incarnation yet, so the link is
  unverified and grants nothing), or `ended` (the parent's live epoch differs, the parent is gone,
  or the link was refused when verified). A new session under the parent's ref never adopts the old
  session's children or their asks.
- `child_owners`: the principals, other than this scope's owner, allowed to register child scopes
  under it (for example Magic Context's historian under a head). Parenting is a separate right from
  carrying: a carrier gets none by being listed.
- `carriers`: who, other than the owner, may open routes under the scope. Each entry is either
  a bare principal, which may open to any module, or `{principal, targets: [module_id, ...]}`,
  which may open only to the listed modules. Targets are module ids, because `route.open` names
  a target module; the principal half is the principal the opener is stamped as. A targeted
  entry holds 1 to 16 targets; an empty list is refused by name. Bare entries are for runners
  that carry a whole session to arbitrary tools (Broca, `subc-mcp`); every other carrier should
  be targeted. For example AFT on a delegating head is `{principal: "reserved:aft", targets:
  ["plexus", "prefrontal-core"]}`, and a provider that only files asks with Prefrontal is
  targeted at `prefrontal-core`. A module whose questions travel back on the route it was called
  on (Cerebellum) is not listed at all. Being a carrier grants nothing on the provider side:
  what a provider does for the session is still decided by the stamp on its own inbound route.
- `attributes`, settable only by the owner, anything else refused by name:
  - `agent_id` (string): the head's agent, on every head scope. It is identity, and the value
    today's `agentProjectId` admission fact already carries.
  - `delegates` (bool, default false): true lets a provider act as that agent. Refused without
    `agent_id`.

**Authority gate.** `agent_id` and `delegates` may be set only by an owner listed
in the daemon config key `scope_authority_owners` (today `["prefrontal-core"]`, the same module
as `admission_facts_carrier_module_id`). A scope from any other owner may carry `kind`,
`parent`, `child_owners` and `carriers` only; a sync that sets a gated attribute from an unlisted
owner is refused by name (`scope_attribute_not_permitted`). `scope_authority_owners` is a
restart-required key: a change takes effect only on a daemon restart, which closes every route, so
no live route keeps an `owner_authorized` stamp its owner has lost. Providers rely on the stamp's
`owner_authorized` alone and keep no copy of the list (section 7).

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
sync from any other connection of the same owner is refused `scope_sync_not_authority`, with one
exception: a connection presenting the owner's current launch nonce takes authority from one
presenting an older launch's nonce. "Current" is the supervisor's active launch for that module
id, the one whose nonce it records as the module's spawn nonce, never merely the newest spawn: a
blue/green candidate is recorded as the swap's candidate token until promotion, so it is not
current and cannot take authority, and a candidate that fails and is rolled back never had it. So
a restarted owner process is never locked out by its predecessor's connection that is wedged but
still open. That
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
- **An ended scope cannot come back.** Within an incarnation, a record naming a tombstoned
  `(owner, ref, scope_epoch)` is refused `scope_epoch_ended`; the owner must use a higher epoch.
  Across a daemon restart the tombstones are gone, so owners persist a removal, with its epoch,
  before syncing it away, and never re-send a removed epoch.
- A stamp is a snapshot taken at bind, so revoking authority ends the routes that carry the old
  stamp, each with its own reason so a carrier can tell them apart:
  - `scope_carrier_removed`: the opener is no longer a listed carrier, or the route's target is
    no longer in its entry's `targets` (widening a list changes nothing live);
  - `scope_delegation_changed`: `delegates` went from true to false, or `agent_id` changed;
  - `scope_ended`: the scope is gone, or replaced by a higher epoch.
  - `scope_parent_ended`: the scope's parent ended (section 3).
  These are new `route.closed` reasons. Older SDKs map an unknown close reason to "do not
  reopen", which is right for the first and third; only carriers that use scopes, which are new
  code, receive any of them.
- A sync that gives a live `(owner, ref)` a higher `scope_epoch` ends the old scope (tombstone,
  drain with `scope_ended`) and creates the new one. A new session never inherits a live route,
  stamp or approval of the old one.
- `parent` is checked against the parent's owner's synced set. When that owner has synced in this
  incarnation, the edge is accepted only if the syncing owner is the parent's owner or is listed in
  the parent's `child_owners` and the named epoch is the parent's live one; otherwise the record is
  refused `scope_parent_not_permitted`. When the parent's owner has not synced yet, which happens
  when owners re-sync in any order after a daemon restart, the child is accepted with
  `parent_state: pending` and the edge is checked when the parent's owner syncs: it becomes
  `linked` if it passes, or `ended` if the parent is absent, at another epoch, or the edge is not
  permitted. A child is never refused, and nothing bound to it deleted, only because the parent's
  owner synced later. A cycle is refused.
- When a parent ends, its children stay and their state becomes `parent_state: ended`. That
  change drains the children's live routes with reason `scope_parent_ended`, so no live route keeps
  a stamp saying the parent is live; a carrier re-opens and gets the current stamp. The change
  from `pending` to `linked` drains nothing. One owner's removal never tears down another owner's
  routes.
- `scope.sync` from `direct` is refused by name.
- **A refused record does not block the rest.** A record refused on its own merits (for example
  `scope_epoch_regressed`, a forged parent, a gated attribute) keeps its previous state and is
  named in the reply with its reason, and the other records apply. The generation rule still
  applies to the sync as a whole: a stale generation refuses everything and changes nothing.

## 4. Opening a route under a scope

`route.open` gains `scope: {owner, ref, scope_epoch}`. Every opener sends `scope_epoch`, the owner
included: an owner forwarding a call names the epoch of the call it serves, so an old inbound call
can never be carried into a newer session that reused the ref. The daemon admits the open only
when the opener is the owner or a listed carrier, and otherwise refuses by name (the full set is
in section 5a):
- `scope_epoch_required`: the open named no epoch. Terminal.
- `scope_not_synced`: the owner has not synced since this incarnation started and is configured.
  Retryable: after a daemon restart a carrier's open can arrive before the owner re-syncs, and
  the carrier waits within its own bound.
- `scope_not_live`: the owner has synced and the ref is not in its set, or the owner is not a
  configured module. Terminal.
- `scope_ended`: the named `scope_epoch` does not match the live record. Terminal.
- `scope_not_carrier`: the opener is neither the owner nor a listed carrier, or it is a targeted
  carrier and the target module is not in its list. Terminal.

There is no relay class: a module that must present a scope onward is listed as a carrier. A carrier
route lives until the carrier closes it or the scope ends or changes as in section 3.

On admission the daemon captures `(scope_epoch, version)` into the pending bind and stamps the
bind with `scope {owner, ref, scope_epoch, kind, parent, parent_state, attributes}`, next to the
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
3. `ended`, or `not_live` with `owner_synced: true`: refuse by name, and the binding is dead. An
   owner's first sync of an incarnation is its full set, so a ref missing from it is ended even if
   its tombstone was lost to a restart or evicted.
4. `not_live` with `owner_synced: false` and `owner_configured: true`: not re-synced yet. Hold,
   and refuse as `scope_unverifiable` past the reader's own bound.
5. `not_live` with `owner_configured: false`: the owner will never sync. Refuse the action.
In cases 2 and 3 the reader may delete what it stores for that binding. In cases 4 and 5 it keeps
it. As a backstop against an owner that never returns, every stored binding also has its own
expiry.

**Two different holds.** A live call that meets case 4 waits within its caller's deadline and at
most 45 s, then refuses `scope_unverifiable`. A stored approval is different: it is kept, not
refused, for as long as its own expiry allows, and it executes only after a later read answers
case 1. The 45 s bound applies to a call waiting now, never to a stored approval.

`scope.subscribe` (deferred to stage 7) is a held request shaped like
`supervisor.spawn_subscribe`, with the same `{daemon_incarnation, seq}` cursor and too-old-cursor
refusal. It sends a snapshot of live scopes, then events:
- `{owner, ref, scope_epoch, created}` when a scope is created;
- `{owner, ref, scope_epoch, ended}` when a scope ends;
- `{owner, ref, scope_epoch, changed}` when its carriers, `delegates`, `agent_id` or `parent_state`
  change;
- `{owner, synced, scopes: [...]}` when an owner's first sync of this incarnation is accepted,
  carrying that owner's full live set.
A scope of that owner the reader holds and not in the `synced` set is ended. Before the owner's
`synced` event, a scope merely missing is held, never treated as ended.

## 5a. Refusals and drains

Every refusal the scope feature adds, in one place. "Retryable" means nothing was sent; the
caller may re-open within its own deadline.

| Code | Where | Meaning | Retryable |
|---|---|---|---|
| `scope_not_synced` | `route.open` | the configured owner has not synced since this daemon incarnation | yes |
| `scope_changed` | `route.open` commit | the record changed between admission and commit | yes |
| `scope_not_live` | `route.open` | the owner synced and the ref is not in its set, or the owner is not configured | no |
| `scope_ended` | `route.open`, admission or commit | the named epoch is not the live one, or the scope ended | no |
| `scope_epoch_required` | `route.open` | the open named no epoch | no |
| `scope_not_carrier` | `route.open` | the opener is not the owner or a carrier, or not targeted at this module | no |
| `scope_unsupported` | carrier, before opening | the daemon does not advertise `scopes/v1` | no |
| `scope_sync_not_authority` | `scope.sync` | the connection is not the owner's sync authority | no |
| `scope_sync_stale` | `scope.sync` | the generation is not larger than the last accepted | no |
| `scope_epoch_regressed` | per record in a sync | the epoch is lower than the one held | no |
| `scope_attribute_not_permitted` | per record in a sync | a gated attribute from an owner not in `scope_authority_owners` | no |
| `scope_parent_not_permitted` | per record in a sync | the parent's owner has synced, and the syncing owner is not the parent's owner or in its `child_owners`, or the parent epoch is not live | no |
| `scope_kind_changed` | per record in a sync | `kind` differs from the held record at the same epoch | no |
| `scope_epoch_ended` | per record in a sync | the `(owner, ref, scope_epoch)` was ended in this incarnation | no |

A hold for `scope_not_synced` is bounded by the opener's own deadline and at most 45 s; a stored
approval is kept, not held, as in section 5. There is no separate daemon-side hold.

Which live routes a record change drains:

| Change in a sync | Routes drained | `route.closed` reason |
|---|---|---|
| scope removed, or replaced by a higher epoch | every route under it | `scope_ended` |
| a carrier entry removed | that carrier's routes | `scope_carrier_removed` |
| a module removed from a carrier's `targets` | that carrier's routes to that module | `scope_carrier_removed` |
| `delegates` true to false | every route under it | `scope_delegation_changed` |
| `agent_id` changed | every route under it | `scope_delegation_changed` |
| `parent_state` becomes `ended` | every route under it | `scope_parent_ended` |
| anything else (`child_owners`, a carrier or target added, `parent_state` from `pending` to `linked`, an unchanged record) | none | |

A route whose stamp names an older `version` but is not in a drained row stays up: its stamp
still grants no more than the current record does.

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
the record, then take the forwarding write lock and publish the new `(scope_epoch, version)`. Every
pending bind tagged with the old version is then refused at commit (`scope_changed`, or
`scope_ended` if the scope ended). Live routes are drained only as the table in section 5a says, on
every endpoint including superseded endpoints of a swap: a change in the "none" row, such as adding
a carrier, leaves every live route and its in-flight calls alone. A commit before the publish is
covered by the drain rule; a commit after it sees the new version and refuses.

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
  the route (section 3), so no new call arrives on it.
- **A drain does not stop a call already delivered.** The daemon never reads request bodies and
  cannot recall a frame it has forwarded; a drained route's in-flight call may still run in the
  module. Whether a provider must re-check the scope (`scope.describe`) before an irreversible or
  delegated action it received before a revocation is an open decision for the operator (triage
  row T19). Until it is ruled, a provider that holds a call past its own reply (an approval, a
  deferred execution) re-checks at the point of execution, which Plexus and Cerebellum already
  do.

## 8. Rollout

Readers first, then writers:
1. Providers that need a scope refuse a bind without a stamp. Today's daemon drops unknown
   `route.open` request fields, so a carrier naming a scope on an old daemon gets an unstamped
   route, which the provider's refusal makes fail closed.
2. The daemon ships `scope.sync`, `scope.describe`, the stamp and `scope_authority_owners`,
   advertised in `server.describe` as capability `scopes/v1`, and `scope_changed` and
   `scope_not_synced` join subc-protocol's retryable `route.open` set in the same release. An
   older SDK treats them as terminal, which is safe. `scope.subscribe` and `scope.patch` follow in
   stage 7 under their own capability.
3. Owners and carriers use scopes only when the capability is advertised. A carrier that cannot
   open a scoped route fails the call (`scope_unsupported`) instead of opening an unscoped one.

## 9. Tests the daemon change must carry

Each fails by name when its rule is removed:
- only the owner or a listed carrier is admitted; `direct` can neither sync nor own;
- an open without `scope_epoch`, the owner's included, is refused `scope_epoch_required`;
- a change of `kind` at the same epoch is refused `scope_kind_changed`; re-sending a tombstoned
  epoch is refused `scope_epoch_ended`;
- a child synced before its parent's owner is accepted as `pending`, becomes `linked` or `ended`
  when the parent's owner syncs, and is never refused for the order; every restart ordering of
  parent and child owners is tested;
- a parent ending drains its children's live routes with `scope_parent_ended`;
- adding a carrier leaves another carrier's in-flight call intact, and a stale pending bind is
  refused at commit;
- a swap candidate never takes sync authority, including one that fails and is rolled back, and
  the serving owner's syncs are accepted throughout;
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
- a parent is accepted only from its owner or a principal in its `child_owners`, and only at its
  live epoch; a carrier of the parent is refused; a cycle is refused; a new session under the
  parent's ref makes existing children read `parent_state: ended`;
- re-sending an unchanged record does not move its `version` or drain anything;
- a record refused in a sync keeps its previous state while the rest applies;
- a connection with the owner's current launch nonce takes sync authority from an older launch's;
- a higher `scope_epoch` for a live ref ends the old scope first, a lower one is refused, and the
  same one re-synced after a daemon restart reads as the same session; `describe` separates the
  five reader cases;
- each revocation drains with its own reason; `owner_authorized` is true only for listed owners;
- a targeted carrier is admitted only to its listed modules, an empty target list is refused,
  and removing a live route's target from the list drains it with `scope_carrier_removed`;
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
4. A parent is accepted only from the parent's owner (and, since r6, a principal in its
   `child_owners`); r2's "serving it" clause let any callee forge a parent link. An earlier wording
   of this item also allowed the parent's carrier, which r6 removed.
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

From the room, after r4:
13. Carrier entries can name their target modules, so a provider listed to file asks under a
    session's scope cannot open to any other module as that session. Without it, removing the
    relay class had only moved the widening from per call to per scope.

From the Athena review of extensibility r7:
14. Non-owner openers must name the epoch (`scope_epoch_required`); r5 made it optional, so a
    carrier that omitted it could be admitted into a newer session under a reused ref.
15. Parents carry their epoch and are granted by `child_owners`, not by carrying; r5 let any
    carrier parent a scope on a head, and gave Magic Context no way to parent at all.
16. Unchanged records don't bump `version`; a refused record doesn't block the sync; a newer launch
    of the owner takes sync authority from a wedged older connection.
17. One refusal table and one drain table (section 5a).
18. `hook_order` left the daemon for `session.plan`; `scope.subscribe` and `scope.patch` are
    deferred to stage 7, and `subscribe` gains `created` events and a full set on `synced`.

From the Athena review of extensibility r7.2:
19. `kind` is fixed per epoch and an ended epoch cannot be re-created (T8, T13).
20. Parent links are verified against the parent's owner's synced set, with a `pending` state for
    any restart ordering, and a parent ending drains its children's routes (T11, T12).
21. Every open names the epoch, the owner's included (T21); `owner_authorized` is the only rule
    and its key is restart-required (T14, T22).
22. "Current launch" means the supervisor's active launch, so a swap candidate never takes sync
    authority (T7).
23. Stored approvals are kept under their own expiry and a live call waits at most 45 s; a full-set
    miss is definitive and may be deleted (T10, T18).
24. The locking prose drains per the table, not every route (T15); the drain-reason prose matches
    the table (T16); section 1 points at the launch-nonce track (T17); the non-authority field list
    includes `child_owners` (T9); a drain does not recall a delivered call (T19, open).
