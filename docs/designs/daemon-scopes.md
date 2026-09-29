# Scopes: owned identity records in the daemon

Status: design r2 (declared attribute set, from the room's review of r1). Nothing here is built. The extensibility design (magic-context
`.cortexkit/alfonso/plans/ck-extensibility-r6-7-amendments.md`, section K2) relies on the
contract in sections 2 to 6; this note is where the daemon side is specified and reviewed.
Settled in the extensibility room ([#416] to [#447]) and ruled by the operator.

## 1. Why

Several modules act on behalf of a session they did not start: Broca runs a head that
Prefrontal owns, AFT relays that head's GitHub writes to Plexus, and Cerebellum holds grants
"for this conversation". Each of those needs to know, from something it can trust, which
session a route belongs to, who owns it, and which agent it acts for. Until now each need had
its own mechanism: a self-asserted `session_ref`, a `parent_session_ref`, an `owner` label
trusted from any module declaring `llm-runner/v1`, and a separate delegation registry. An
Athena review showed that each one could be claimed by a caller that did not own it.

A scope replaces all of them with one record the daemon holds and stamps. It generalises what
the daemon already does for admission facts (`control.rs:2937-2974`): only a configured carrier
may attach them, and the daemon stamps them on the bind without reading them.

## 2. The record

A scope is identified by the pair `(owner, ref)`:
- `owner`: the principal that registered it, taken from the registering connection's stamp,
  never from the request. `direct` cannot own a scope (section 3).
- `ref`: an opaque string chosen by the owner, unique within that owner only. Two owners may
  use the same ref without colliding, and no module can squat or block another owner's ref.

Fields:
- `kind`: a closed enum, `head | worker | ephemeral`. Providers read it; they never derive it
  from other fields.
- `parent`: optional `(owner, ref)` of another scope.
- `carriers`: the principals, other than the owner, that may open routes under the scope.
- `attributes`: a small typed set, settable only by the owner. Carriers and relays set nothing.
  A key outside the declared set is refused by name, so a typo cannot pass silently to
  providers. The declared set:
  - `agent_id` (string): the head's agent, on every head scope, delegating or not. It is
    identity: Cerebellum keys durable browser profiles and remembered app grants on it. It is
    the value today's `agentProjectId` admission fact already carries, so nothing re-keys.
  - `delegates` (bool, default false): true means a provider may act as that agent (Plexus bot
    writes). The daemon refuses `delegates: true` on a scope without `agent_id`. A provider
    acts as the agent only when `delegates` is true and `agent_id` matches; `agent_id` alone is
    never permission to act.
  - `hook_order` (closed enum, today only `exclusive`): the launch turned off the user's own
    hook settings, so the CK plugin is the only hook on a call. Absent means no guarantee.
  Per-provider policy, such as which browser controls a session may use, is not an attribute:
  the provider keys it on `kind`.

Bounds: at most 10,000 live scopes and 1,000 tombstones per owner, and 4 KiB of attributes per
scope. Past a bound the request is refused by name; the daemon never evicts a live scope.

## 3. Registering: `scope.sync`

An owner sends its full set: `scope.sync {generation, scopes: [...]}`.
- The first sync on a connection is always a full replace, whatever its generation. Later syncs
  on the same connection must carry a larger generation, and a smaller one is refused as stale.
  So an owner that restarts, or a daemon that restarts, can never lock a head out: nothing about
  the generation needs to survive either.
- A scope present before and absent now is ended: it is tombstoned (section 6) and every route
  under it is drained with reason `scope_ended`.
- `parent` is admitted only if the owner is at that moment the owner of the parent, a carrier
  of it, or serving it through a live route (section 4). So a parent link cannot be forged.
- When a parent ends, its children stay. Their stamp carries `parent_ended: true`. Cascading
  would let one owner's removal tear down another owner's routes.
- When the owner's connection closes, its scopes stay live until the owner's next first sync
  replaces them. A Prefrontal restart therefore does not cut off running Broca sessions.
- `scope.sync` from `direct` is refused by name. `direct` is one principal shared by every
  harness plugin, `ck`, and any local process holding the connection file, so owning a scope
  would prove nothing. Standalone hosts (OpenCode or Pi without Prefrontal) run without scopes.

`delegation.sync` from the earlier design is not built; delegation is the `agent_id` attribute
of a head scope.

## 4. Opening a route under a scope

`route.open` gains `scope: {owner, ref}`. The daemon admits it when the opener is:
- the owner;
- a listed carrier; or
- a relay: a module that has a live inbound route under the same scope at that moment.

Otherwise it refuses by name (`scope_not_live`, `scope_not_carrier`). A carrier route lives
until the carrier closes it or the scope ends. A relay route is tied to the inbound route that
admitted it (its basis): when the basis closes, the relay route is drained with reason
`scope_basis_ended`. The basis is a route, never a call, since the daemon does not read calls.

On admission the daemon stamps the bind with `scope {owner, ref, kind, parent, parent_ended,
attributes, chain}`, next to the principal it already stamps. `chain` lists the relays between
the owner or carrier and this route. The immediate opener stays in the principal field, so a
provider's own first-party checks are unchanged. Providers read identity only from the stamp.

## 5. Reading: `scope.describe` and `scope.subscribe`

`scope.describe {owner, ref}` answers:
- `status`: `live`, `ended` (tombstoned since this daemon incarnation) or `not_live`;
- `daemon_incarnation`;
- `owner_synced`: whether this owner has sent a sync since this incarnation started;
- `owner_configured`: whether the owner is a module in the supervisor's roster;
- for `live`, the same fields as the stamp.

That lets a reader act safely after a restart:
- `live`: use it.
- `ended`, or `not_live` with `owner_synced: true`: refuse by name. A tombstone evicted by the
  bound reads as `not_live`, which also refuses.
- `not_live` with `owner_synced: false`: not re-synced yet. Hold, and refuse as
  `scope_unverifiable` past the reader's own bound.
- `owner_configured: false`: the owner will never sync; refuse at once.

`scope.subscribe` is a held request shaped like `supervisor.spawn_subscribe`: a snapshot of live
scopes with `owner_synced` per owner, then events `{owner, ref, ended}` carrying the
`{daemon_incarnation, seq}` cursor, and the existing too-old-cursor refusal that sends the
reader back to a snapshot. A provider holding state that outlives routes (Cerebellum's
conversation grants) drops it only on an explicit `ended` event. A scope merely absent from a
snapshot before its owner re-syncs is held, never treated as ended. It needs no bus.

## 6. State, locking and restarts

All state is in memory:
- the scope table, keyed `(owner, ref)`;
- per-owner sync state: the connection of the last sync, its generation, and `owner_synced`;
- tombstones since this incarnation, per owner, oldest evicted first;
- an index from scope to the live routes under it, and from route to the relay routes it is
  the basis of.

Lock order is scope table first, then the forwarding table. Admission checks the scope under
the scope-table read lock, and the bind commit re-checks it (and, for a relay, its basis) under
the forwarding write lock before the route becomes routable. So a scope ended, or a basis
closed, between the check and the commit refuses the open instead of leaving a route under a
dead scope. Ending a scope flips its state under the scope-table write lock, then collects its
routes under the forwarding lock and drains them.

A daemon restart ends every route anyway, so nothing needs a scope from before it: the table
starts empty, `owner_synced` is false for everyone, and readers hold as in section 5. The
in-place upgrade (`docs/designs/daemon-in-place-upgrade.md`) keeps routes across the exec, so
the scope table, tombstones, sync state and `daemon_incarnation` join the state handed to the
new process. An upgrade then does not look like a restart to readers.

## 7. Rollout

Readers first, then writers:
1. Providers that need a scope refuse a bind without a stamp. Today's daemon drops unknown
   fields in a `route.open` request, so a carrier naming a scope on an old daemon gets a route
   with no stamp; the provider's refusal makes that fail closed.
2. The daemon ships `scope.sync`, `scope.describe`, `scope.subscribe` and the stamp, advertised
   in `server.describe` as capability `scopes/v1`.
3. Owners and carriers use scopes only when the capability is advertised. A carrier that cannot
   open a scoped route fails the call (`scope_unsupported`) instead of opening an unscoped one.

## 8. Tests the daemon change must carry

Each fails by name when its rule is removed:
- an opener that is neither owner, carrier nor relay is refused; each of the three is admitted;
- `direct` cannot sync;
- the same ref under two owners is two scopes;
- a relay route drains when its basis closes, and a carrier route does not;
- ending a scope drains every route under it; an open racing the end is refused at commit;
- a forged parent is refused; a parent's end leaves children with `parent_ended`;
- a restarted owner's first sync replaces its set at any generation; a stale later sync is
  refused;
- `describe` distinguishes `live`, `ended` and `not_live`, with correct `owner_synced` and
  `owner_configured`, across a daemon restart;
- `subscribe` delivers `ended` events and resumes from a snapshot after a too-old cursor;
- an attribute outside the declared set and each bound are refused by name.

## 9. Not in this note

- Which sessions get scopes, and the carrier sets per harness: Prefrontal's rules, in K2.
- Owner-scoped data (late results, persona text): providers store it with the owner and serve
  it only to that verified principal; the daemon only supplies the verified identity.
- A per-process carrier token was considered and not built: a same-user process can read
  another's environment, so it would narrow mistakes without adding a boundary. For local
  processes a scope proves ownership and delegation, not which process is calling.
