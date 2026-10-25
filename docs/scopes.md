# Scopes: owned identity records in the daemon

This document describes current behaviour. Every operation here is built except
`scope.subscribe`; until it exists, providers read `scope.describe`. Sections 2 to 6 define
the record, authority, admission, reader and locking contracts that scope owners, carriers and
providers rely on; section 9 lists the tests that pin them, and section 11 records details the
implementation settled. The daemon advertises `scopes/v1` when these operations are available.

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

Scopes prevent accidental identity misuse but do not stop another process running as the same user from taking a module's identity until its launch nonce is delivered over an inherited descriptor and hardened-runtime signing prevents debugger access; see `docs/launch-nonce.md`, §1.

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
  A sync or apply that re-sends a record unchanged does not move it.
  It is how a bind notices a change between admission and commit (section 4).

Fields:
- `bind_proof_sha256`: optional SHA-256 of the scope's 32 secret bytes, encoded as
  exactly 64 lowercase hexadecimal characters. Accepted in both `scope.sync` and
  `scope.apply`; malformed hashes refuse decoding/validation. The owner mints the
  secret and gives it only to the host that registered the session. Only the
  holder of the scope's bind proof can bind it, in addition to the existing owner,
  carrier and epoch rules. A new epoch replaces the hash and still closes the old
  epoch's routes. The daemon stores only the hash, never the proof.
- `kind`: a closed enum, `head | worker | ephemeral`, fixed for the life of an epoch. A sync that
  changes it at the same epoch refuses that record `scope_kind_changed`; the owner uses a higher
  epoch, which ends the old scope.
- `workdir`: optional absolute session folder (the checkout or worktree root for git).
  The daemon canonicalizes it like `route.open`'s `project_root`, resolving symlinks
  and allowing a missing tail. Relative or unresolvable paths refuse that record
  as `invalid_workdir`.
- `project_id`: optional opaque Entorhinal project id, 1–128 printable non-space ASCII
  bytes (`0x21`–`0x7E`). Malformed ids refuse the whole sync or apply as
  `invalid_control_body`. A folder with no registered project needs only `workdir`.
  Both fields are fixed for the epoch: adding, changing or removing either is
  `scope_workdir_changed` or `scope_project_changed`, respectively. Canonical spellings
  of the same folder compare equal. Use a higher epoch to change them.
- `expires_at_ms`: optional absolute Unix wall-clock milliseconds, fixed for the life of an
  epoch. Omitted means no daemon-enforced expiry. It is a record field, not an attribute;
  any owner may set it, for any kind (see the expiry subsection).
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
  - `on_behalf_of_agent` (optional string): attribution only, never authority. A provider may
    use it to choose identity-scoped presentation (bot choice, read budgets, bound repositories)
    and for logging. It must never treat it as authority to act as that agent, hand out the
    agent's per-agent handles or consent on its strength, or treat it as `agent_id`.
    Worker scopes can name their head agent here without acquiring the head's authority.
  - `delegates` (bool, default false): true lets a provider act as that agent. Refused without
    `agent_id`.
  - `flow_id` (optional string): this scope belongs to the named flow (section 2's
    flow identity subsection).
  - `run_id` (optional string): this `ephemeral` scope belongs to one agent run (section 2's
    agent-run identity subsection).

**Authority gate.** `agent_id`, `on_behalf_of_agent`, `delegates`, `flow_id` and `run_id` may be set only by an owner listed
in the daemon config key `scope_authority_owners` (today `["prefrontal-core"]`, the same module
as `admission_facts_carrier_module_id`). A scope from any other owner may carry `kind`,
`parent`, `child_owners`, `carriers`, `expires_at_ms`, `workdir` and `project_id`, with empty attributes; a sync or
apply that sets a gated attribute from an unlisted owner is refused per record by name
(`scope_attribute_not_permitted`). `expires_at_ms` is not gated by this list. `scope_authority_owners` is a
restart-required key: a change takes effect only on a daemon restart, which closes every route, so
no live route keeps an `owner_authorized` stamp its owner has lost. Providers rely on the stamp's
`owner_authorized` alone and keep no copy of the list (section 7).

`workdir` and `project_id` are attribution, never authority. Owners send them after
the daemon advertises `scope-workdir/v1` in `server.describe`. An owner sets
`project_id` only to the `projectId` that Entorhinal's `resolve {path: workdir}`
returns for the same folder, and leaves it out when Entorhinal has no project for
that folder. It never invents an id or copies one from another session, because
every reader groups the session under that project. The daemon can't check this:
it treats the id as opaque and never calls Entorhinal, which it supervises. A reader
that needs certainty resolves the `workdir` itself. The daemon includes
them in a `route.bind` scope stamp only when the target manifest's
`capabilities.provides` declares `scope-workdir/v1`; all other targets receive the
same bind bytes as before. Bound scoped routes contribute their workdir to
`supervisor.live_roots` alongside the legacy bind roots. Their activity entries
carry the scope's `workdir` and `project_id`; existing activity fields are unchanged.

`on_behalf_of_agent` uses the agent token rule: 1–256 printable, non-space ASCII bytes
(`0x21`–`0x7E`). Malformed tokens refuse the whole sync or apply as `invalid_control_body`.
It is mutually exclusive with `agent_id`, `delegates: true` and `flow_id`; `run_id` requires
`agent_id`, so it is excluded too. Conflicting combinations refuse that record in sync or
apply as `scope_attribute_not_permitted`. Adding, changing or removing attribution within
an epoch increases the content version and closes routes with `scope_delegation_changed`.
An unchanged resend does neither.

Owners send this attribute only after the daemon advertises `scope-on-behalf-of/v1` in
HELLO_ACK `subc_capabilities` or `server.describe`. The daemon sends it in `route.bind`
stamps only to targets whose captured manifest provides that same capability, stripping it
for every other target (including one declaring only `call-modes/v1`). Unlike flow or run
identity, removing attribution cannot grant agent authority: the scope has no `agent_id`
or delegation. Declaring the capability promises attribution-only handling, not merely
the ability to decode the field. The same capability gate protects describe readers:
only a calling module declaring it receives the attribute (section 5).

Bounds: at most 10,000 live scopes per owner and 4 KiB of attributes per scope; past either the
sync or apply is refused by name and nothing is applied. Tombstones (section 6) are capped at
1,000 per owner and evicted oldest first; that bound never refuses.

### Expiry: `expires_at_ms`

The daemon ends a live scope when its absolute deadline is at or before the current wall clock.
Expiry has the same effects as omission from a full sync: the ref@epoch is tombstoned, its
routes close with `scope_ended`, and its children stay live with `parent_state: ended` and
their routes drain with `scope_parent_ended`. There is no new close reason. Each expiry logs a warning with
the owner, ref, epoch, deadline and number of routes closed.

A new epoch's deadline must be in the future and at most `MAX_SCOPE_EXPIRY_AHEAD_MS`
(86,400,000 ms, 24 hours) ahead. A passed deadline is refused per record as `scope_expired`;
a deadline more than 24 hours ahead, including `u64::MAX`, is `scope_expiry_too_far`.
Changing, adding or removing the deadline on a live epoch is `scope_expiry_immutable`,
even if the proposed value is past or beyond that horizon; the stored value stays unchanged.
An unchanged deadline is not re-checked against the horizon. To end early, omit the scope
from a full sync or name its ref@epoch in `scope.apply`'s `end`. To use a different deadline,
register a higher epoch; there is no shortening or extension within an epoch.

The evaluation points are:
- **Periodic sweep:** while any live scope has a deadline, the daemon sweeps at most 1 s
  apart. It judges by Unix wall time, not monotonic elapsed time; a deadline passed during
  sleep is ended at the first sweep after wake, at most 1 s later. The sweep works even
  after the authority connection closes and changes neither authority nor `last_generation`.
- **Accepted `scope.sync` or `scope.apply`:** under the scope write lock, the daemon sweeps
  only the calling owner's expired scopes before classifying entries. Every whole-call
  check must pass, including the resulting live-count limit; a whole-call refusal commits
  nothing, including no sweep. A scope ended by this call's sweep appears once in its
  `ended` reply. Re-sending that epoch is `scope_expired`, explicitly ending it is
  `not_live`, and a higher-epoch upsert is `created`, not `replaced`. A per-record refusal
  does not undo an accepted call's sweep.
- **`route.open` admission:** after finding the live epoch and before checking the carrier,
  a passed deadline refuses the open as `scope_not_live`, naming the ref and passed deadline.
  Admission does not mutate the table, and `scope_expired` is not a route-open code.

`scope.describe` does not evaluate expiry: it may still report a past-deadline scope as `live`
until a sweep. It returns `expires_at_ms` when set; the bind stamp carries that deadline only
to a target declaring `call-modes/v1` (section 4a). The deadline is fixed per epoch, so the
bind-time value stays exact. No expiry push or backlog is sent to the owner; it learns
through describe, `scope_expired`, or `ended` in a call whose own sweep ended the scope.
A periodic sweep's ends do not appear in a later
call's `ended` reply.

Expiry tombstones retain their cause: re-sending that ref@epoch, even without a deadline,
is `scope_expired` while the tombstone is held. Omission, an explicit end or replacement
before the deadline creates an owner-ended tombstone and yields `scope_epoch_ended` instead.
Tombstones are in-memory and share the 1,000-per-owner oldest-first bound. A restart or
eviction can lose them, but a fresh store still refuses a passed deadline. Owners must
therefore re-send each record with its original deadline; an absent or future deadline
can be accepted after tombstone loss. Moving wall time backwards delays a not-yet-ended
scope but never revives an ended one while its tombstone is held.

### Flow identity: `flow_id`

The optional `attributes.flow_id` means "this scope belongs to flow X". The
scope's owner (prefrontal), and only an owner in `scope_authority_owners`, may set
it; an unlisted owner receives `scope_attribute_not_permitted`, just as for
`agent_id`. The daemon stamps it verbatim in `ScopeStamp.attributes` on
`route.bind`. Providers use it to distinguish a flow's routes from a head's,
and treat a non-owner opener on a flow scope as the flow's carrier.

It is 1–256 printable non-space ASCII bytes, checked with the shared opaque-token
validator. A malformed value refuses the whole sync or apply as `invalid_control_body`, naming
`flow_id`. Scope refs retain their existing acceptance rule. `flow_id` alone,
without `agent_id` and with `delegates: false`, is valid; `delegates` still requires
`agent_id`.

The bind stamp's flow identity is fixed for that route. As with `agent_id`, a
same-epoch change (including adding or removing `flow_id`) is accepted, bumps the
scope's content version, and drains every route under it with
`scope_delegation_changed`. New binds carry the new value; an unchanged re-sync
neither bumps the version nor drains routes.

### Agent-run identity: `run_id`

The optional `attributes.run_id` identifies one run on behalf of `agent_id`; run scopes use
the existing `ephemeral` kind, not a new kind. Like `flow_id`, it is 1–256 printable non-space
ASCII bytes. A malformed token refuses the whole sync or apply as `invalid_control_body`,
naming `run_id`. Only an owner in `scope_authority_owners` may set it; an unlisted owner gets
`scope_attribute_not_permitted` even when the scope has a permitted expiry.

After the existing delegates-without-agent check, the daemon refuses a record with:
- `run_id` but no `agent_id`: `scope_run_id_without_agent`;
- both `run_id` and `flow_id`: `scope_run_id_with_flow_id`.

A run record may set `delegates: true`; the run-authority rule is enforced at route admission
and relay re-check instead of record validation. A delegating run binds only to a module
declaring `call-modes/v1`. A non-delegating run may also bind to a legacy
`agent-run-scopes/v1` target, which was written assuming a run never carries its agent's
authority (sections 4 and 4a). `scope_run_id_delegates` remains a stable code for older peers,
but this daemon's record validation no longer emits it. The daemon stamps `run_id` verbatim
in `ScopeStamp.attributes` on admitted binds. Changing, adding or removing it on a live
epoch is accepted, bumps `version` and drains every route under the scope with
`scope_delegation_changed`. An unchanged resend does neither. A scope with only `flow_id`
retains its existing flow rules and capability gate.

## 3. Registering: `scope.sync` and `scope.apply`

An owner sends its full set: `scope.sync {generation, scopes: [...]}`. Once an accepted full
sync has made this connection the authority, it may send a delta instead:
`scope.apply {generation, upsert: [ScopeRecord, ...], end: [{ref, scope_epoch}, ...]}`.
Apply changes only the named refs, uses the full sync's per-record checks, and shares its
generation sequence. It never takes authority or resets the generation baseline; a connection
that would need to take authority must first send a full sync, or gets `scope_sync_required`.
An owner with a large set can register or end one scope without re-sending thousands.

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
until cutover. Promotion changes no scope-table authority or set: the promoted connection's
first accepted full sync takes authority. An apply from it before that sync is
`scope_sync_required`; the superseded connection is refused `scope_sync_not_authority`.
When the authority connection closes, authority is free, and the next accepted full sync
from that owner takes it, never an apply.

**Generations.** A sync or apply from the authority must carry a generation larger than the last
accepted; an equal or smaller one is `scope_sync_stale`, and a whole-call refusal changes nothing.
The first sync from a connection that has just taken authority is a full replace at any generation,
and its generation becomes the new baseline. So a restarted owner, or an owner after a daemon
restart, is never locked out, and a stale connection can never overwrite a newer one.

**Effect of a sync.**
- An accepted call first sweeps the calling owner's expired scopes (section 2); it does not
  sweep another owner's scopes.
- A scope present before and absent now is ended: tombstoned, and every route under it drained
  with reason `scope_ended`.
- **An ended scope cannot come back.** Within an incarnation, a record naming a tombstoned
  `(owner, ref, scope_epoch)` is refused `scope_epoch_ended`, or `scope_expired` if expiry ended
  it; the owner must use a higher epoch.
  Across a daemon restart the tombstones are gone, so owners persist a removal, with its epoch,
  before syncing it away, and never re-send a removed epoch.
- A stamp is a snapshot taken at bind, so revoking authority ends the routes that carry the old
  stamp, each with its own reason so a carrier can tell them apart:
  - `scope_carrier_removed`: the opener is no longer a listed carrier, or the route's target is
    no longer in its entry's `targets` (widening a list changes nothing live);
  - `scope_delegation_changed`: `delegates` went from true to false, or `agent_id`, `on_behalf_of_agent`, `flow_id` or `run_id` changed;
  - `scope_ended`: the scope is gone, expired, or replaced by a higher epoch.
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
  `scope_epoch_regressed`, a forged parent, a gated attribute) keeps its post-sweep state and is
  named in the reply with its reason, and the other records apply. An expiry sweep committed by
  the accepted call is not undone by a record refusal. The generation rule still
  applies to the sync as a whole: a stale generation refuses everything and changes nothing.

### Applying a delta

`scope.apply` is a module control request on the owner's registered module connection, just
like sync. Whole-call refusals are error frames; they change neither the set nor generation
and run no sweep. Checks run in this order:
1. No module registration: `not_registered`.
2. The connection fails the launch-nonce authority check: `scope_sync_not_authority`.
3. It is not already the authority through an accepted full sync: `scope_sync_required`.
4. `generation <= last_generation`: `scope_sync_stale`.
5. An empty ref, a duplicate in either list, a ref in both lists, or a malformed `run_id`
   or `flow_id`: `invalid_control_body`, naming the ref or field. More than 10,000 entries
   in either list is `scope_live_limit_exceeded`; an upsert with more than 4 KiB of
   attributes is `scope_attributes_too_large`.

Otherwise the calling owner's expiry sweep and per-record upserts are evaluated together
with the named ends. An upsert yields `created`, `replaced`, `updated`, `unchanged` or
`refused`, with the same codes as sync. An end removes only the exact live ref@epoch; its
outcome is `ended`. Any other end, including a ref the sweep just ended, is `not_live`,
not an error. Unnamed scopes stay live. Parent-link and cycle checks use the post-apply set:
held scopes after the sweep, minus matching ends, plus accepted upserts.

If that resulting set exceeds 10,000 live scopes, the entire call is
`scope_live_limit_exceeded`, with nothing committed, including the sweep. At the cap, one
matching end plus one new upsert succeeds; so does one new upsert when an expired held scope
frees a slot. An explicit end has every effect omission has: `scope_ended` route drains,
child links becoming `ended`, and tag publication. Children remain live.

The reply is `{generation, results, end_results, ended}`. `results` contains a
`ScopeRecordResult` for each upsert in request order; `end_results` contains
`{ref, scope_epoch, outcome}` for each end in request order. `ended` lists every scope the
call ended, each ref at most once, including swept scopes and replaced epochs but not their
children. An accepted call advances generation even if every upsert is refused, every end
is `not_live`, or both lists are empty. A later full sync still ends every scope it omits.

HELLO_ACK advertises `scope.apply` in `subc_ops`. That op also signals support for
`expires_at_ms`; older daemons reject that record field. `ModuleHandle::scope_apply` returns
`ScopeCallError::NotSupported { op: "scope.apply" }` without sending anything when the op
is absent. Whole-call error frames become `ScopeCallError::Refused`; per-entry refusals and
`not_live` stay in the reply. An unexpected reply variant is `ScopeCallError::Protocol`.

## 4. Opening a route under a scope

`route.open` accepts `scope: {owner, ref, scope_epoch, bind_proof?, hold_presence?}`. `bind_proof` is
base64url of the 32 secret bytes without padding. The daemon decodes the proof,
hashes those bytes with SHA-256 and compares digest bytes in constant time, at
admission and again before sending the captured relay bind, but only when the
opener's daemon-stamped principal is not `reserved`. Reserved modules, including
the owner and listed reserved carriers, are attested by their launch nonce and
need no proof; any proof they send is ignored. Direct and other non-reserved
openers must present a matching proof under a hashed scope. A record without a hash
behaves exactly as before: a selector's proof, even malformed, is ignored. Hosts
may therefore send proofs unconditionally once they hold one. Proofs never enter
logs, refusal detail, `scope.describe`, module `route.bind` stamps or activity
attribution; the hash also stays out of describe replies and bind stamps.
Scope owners should set `bind_proof_sha256` only when the daemon they are
connected to advertises `scope-bind-proof/v2` in HELLO_ACK or `server.describe`.
The daemon also advertises v1 for compatibility; v1 alone does not promise the
reserved-opener exemption. Older daemons may reject records carrying the hash.

### Idle-only rotation and connection-held presence

An owner may set `replace_only_if_idle: true` on a record in `scope.sync` or
`scope.apply` to rotate its epoch or change its bind-proof hash without taking
over a live session. If the ref is live and the proposed record has a higher
epoch or changes `bind_proof_sha256` at the same epoch, the daemon refuses that
record while the current epoch has any non-reserved opener's bound route,
admitted bind still in flight, or connected presence holder. Reserved openers'
routes do not show whether the host is alive and do not count. The per-record result is `outcome:
"refused"`, `code: "scope_epoch_in_use"`; its message says routes are still bound
without counting or naming them. The old record and routes stay untouched;
other records apply independently. Created and unchanged records are never
refused by this condition. The flag is a request condition, not stored scope
content; absent or false preserves unconditional replacement and its usual
route drains.

Admission of non-reserved openers reserves epoch usage under the same scope-table write lock that
checks and applies conditional records. The reservation remains counted while
egress is waiting, while the module handles the bind, and after commit until
route closure. Failed or cancelled binds release it. No open can enter between
the idle check and the change; ordinary tag/proof rechecks still settle binds
against unconditional updates.

A host can send `hold_presence: true` in its selector to keep its client
connection present for that scope epoch after a successful admitted bind.
In-flight binds already block rotation; refused opens create no lasting
presence. Presence survives route closure, including a target module restart,
so another local process cannot take over during the host's rebind window.
Closing the holder's connection removes presence. Replacing, ending or
expiring the epoch discards its holders; a daemon restart clears all in-memory
state and all connections. Presence is capped at eight distinct connections
per epoch (including provisional holders during binds); additional opens still
bind but do not add presence. Repeated opens on a holder's connection share its
entry. A reserved opener's `hold_presence` records no presence, even after a
successful bind.

Both fields require `scope-replace-if-idle/v1`, advertised in HELLO_ACK and
`server.describe`. Owners send the condition and hosts send `hold_presence`
only after seeing that capability: older daemons reject unknown record and
selector fields. `scope_epoch_in_use` is terminal, not an automatic retry
instruction; the owner decides when to attempt registration again.

Every opener sends `scope_epoch`, the owner
included: an owner forwarding a call names the epoch of the call it serves, so an old inbound call
can never be carried into a newer session that reused the ref. The daemon admits the open only
when the opener is the owner or a listed carrier, and otherwise refuses by name (the full set is
in section 5a):
- `scope_epoch_required`: the open named no epoch. Terminal.
- `scope_bind_proof_required`: a non-reserved opener's record has a hash but its selector has no proof. Terminal.
- `scope_bind_proof_mismatch`: a non-reserved opener's proof is not valid unpadded base64url of 32 bytes,
  or its SHA-256 differs from the record's hash. Terminal; clients never retry it.
- `scope_not_synced`: the owner has not synced since this incarnation started and is configured.
  Retryable: after a daemon restart a carrier's open can arrive before the owner re-syncs, and
  the carrier waits within its own bound.
- `scope_not_live`: the owner has synced and the ref is not in its set, or the owner is not a
  configured module, or the live epoch's deadline has passed. Terminal. A deadline refusal
  names the ref and deadline and does not mutate the table.
- `scope_ended`: the named `scope_epoch` does not match the live record. Terminal.
- `scope_not_carrier`: the opener is neither the owner nor a listed carrier, or it is a targeted
  carrier and the target module is not in its list. Terminal.
- `target_flow_unsupported`: the scope carries `flow_id`, but the target's
  registered manifest provides neither `call-modes/v1` nor `flow-scopes/v1` in
  `capabilities.provides`. Terminal. Declaring either capability promises that the module
  recognises the field and applies flow behaviour: it never treats a flow as its owner agent
  for approvals, writes or grants. A protocol-version declaration alone does not
  make that promise. The refusal names the target and the capability; nothing
  is sent to the module and no bind is relayed. Never remove `flow_id` to
  accommodate an unsupported target: that would make the flow look like its
  owner's ordinary session. Scopes without `flow_id` and unscoped routes keep
  their existing admission.
- `target_agent_run_unsupported`: the scope carries `run_id`, but the target's registered
  manifest provides neither `call-modes/v1` nor `agent-run-scopes/v1` in
  `capabilities.provides`, or the run delegates and the target does not provide
  `call-modes/v1`. Terminal; the message names the target and capability requirement,
  including delegation when that is the reason, and no bind is relayed. Never drop `run_id`
  to bypass the gate. Scopes without `run_id` and unscoped routes do not require the run gate.

Both gates run at admission and again against the connection captured for relay; a failed
re-check releases the reservation. The captured endpoint also determines which new bind
fields can be emitted, even if the earlier lookup by module id found a different manifest.

There is no relay class: a module that must present a scope onward is listed as a carrier. A carrier
route lives until the carrier closes it or the scope ends or changes as in section 3.

On admission the daemon captures `(scope_epoch, version)` into the pending bind and stamps the
bind with `scope {owner, ref, scope_epoch, kind, parent, parent_state, attributes}`, next to the
principal it already stamps. The immediate opener stays in the principal field. The stamp also
carries `owner_authorized`, computed by the daemon: true when the owner is listed in
`scope_authority_owners`. Providers check that flag rather than keeping their own copy of the
list.

The stamp includes `attributes.run_id` when set, and `expires_at_ms` when set and the
captured target declares `call-modes/v1`. Run and flow binds are capability-gated;
`attributes.on_behalf_of_agent` is included only for `scope-on-behalf-of/v1` targets;
`scope.describe` returns the attribution field only to calling modules declaring that
capability; other attributes and the deadline keep their existing behavior.

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

## 4a. Call modes

`route.bind.call_mode` is an optional field beside `scope`, stamped by the daemon, never
claimed by the caller. It is `codemode` for a scope carrying `attributes.run_id`, `flow` for
one carrying `attributes.flow_id`, and `classic` for a plain scope or no scope. The shared
`derive_call_mode` function supplies this rule to both the daemon and the Rust SDK.

A target whose captured relay endpoint declares `call-modes/v1` (a *declarer*) receives
`call_mode` on every bind, including classic binds. Declaring the capability promises to
read the mode on every bind and handle all three known values. A provider must refuse an
unknown mode; the wire enum preserves unknown strings as `CallMode::Unknown` rather than
failing decoding.
No other target receives `call_mode` or the stamp's `expires_at_ms`, because a reader that
predates a field may reject it and stop serving. Field selection uses the captured endpoint,
not the admission lookup by module id.

During migration, a non-delegating run binds to a target declaring either `call-modes/v1`
or `agent-run-scopes/v1`, and a flow binds to either `call-modes/v1` or `flow-scopes/v1`.
A legacy target still receives the existing scope attributes, but neither new bind field,
even when the scope has a deadline. A target declaring neither matching capability is
refused. A delegating run record is accepted, but its route binds only to `call-modes/v1`:
the run-authority rule has moved from record validation to admission and relay re-check.
The legacy capabilities remain supported in this release and retire only after a fleet
census in a later release.

For an older daemon that admits a dual-declaring provider through its legacy capability
and omits `call_mode`, the Rust SDK derives `RouteBindRequest.call_mode` from the bind's
scope: codemode for a run, flow for a flow, classic otherwise. A missing field is not a
blanket classic default. A daemon that predates call modes rejects `run_id` together with
`delegates: true` during record validation, so it never sends a delegating run bind.

Mode is separate from the caller's role, selected by core as the tool-call `preset` and
unchanged by this release. Providers enforce each tool's permitted modes and refuse
unsupported combinations: the daemon never reads data-plane request bodies or tool names, so it
cannot enforce per-tool modes. The providers' design lives in
prefrontal's docs/designs/call-modes.md.

## 4b. Codemode runs

A codemode run is one program that a carrier module runs for one agent, calling tools for that
agent until the program finishes. Each run has its own scope, a *run scope*: a scope carrying
`attributes.run_id`, on which every bind to a `call-modes/v1` provider is stamped
`call_mode: codemode` (section 4a). This section is the contract for providers that serve
those calls.

**Who does what.**
- The run's owner, a module listed in `scope_authority_owners`, registers and ends run scopes.
  Only such an owner may set `agent_id` and `run_id` (section 2). A carrier asks the owner for a
  run under its own module identity, never the agent's, and the owner decides whether the agent
  may have one.
- The carrier opens one route per provider on the run scope and calls tools on those routes. It
  never widens the scope and never calls a provider the run does not include.
- The daemon admits only the owner and listed carriers, stamps every bind and drains routes as
  section 5a says. It never reads tool names or request bodies, so it cannot police which tools
  a run calls.
- The provider decides each call from its route's bind stamp alone.

**The record.** A run scope is an ordinary scope record. The daemon enforces the attribute rules
of section 2; the rest is how the owner registers a run:
- `kind: ephemeral`;
- `attributes.agent_id`, the agent the run acts for, and `attributes.run_id`, the run. A
  `run_id` needs an `agent_id` and cannot sit beside a `flow_id`;
- `carriers`: the carrier module alone, targeted at the run's providers and the module that
  serves its model calls. A targeted entry lists at most 16 modules;
- `parent`: the scope of the head that started the run, for attribution only. It grants nothing.
  The run's owner opens runs only for heads, so a run's parent is always a head scope, and a
  head scope always carries `attributes.agent_id`. Worker scopes carry none;
- `expires_at_ms`: the run's deadline, fixed for the epoch and at most 24 hours ahead;
- `delegates`: either value is accepted. A delegating run binds only to providers declaring
  `call-modes/v1`, which act as the agent only under the checks in section 7.

**Which tools a run may call.** A provider serves codemode runs by declaring `call-modes/v1`
and listing `codemode` in the catalog `modes` of each tool that serves them. A tool whose
catalog entry has no `modes` serves classic calls only. A caller building a codemode program
offers it only tools whose `modes` contain `codemode`, from providers declaring `call-modes/v1`.
A provider that declares only the legacy `agent-run-scopes/v1` is treated as serving codemode on
all its tools until the fleet census retires that capability (section 4a); it receives only
non-delegating runs and no `call_mode` field. The provider, not the daemon, refuses a call to a
tool that does not list the bind's mode, with a terminal `call_mode_unsupported` error whose
detail is `{tool, mode}`.

**Lifetime.** A run scope ends, and the daemon drains its routes with `scope_ended`, at the
first of:
- the owner ends it, by leaving it out of a full sync or naming it in `scope.apply`'s `end`,
  for example when the carrier reports the run finished;
- its `expires_at_ms` passes (section 2);
- the owner replaces it with a higher epoch (a parent re-point is not a new run epoch);
- the owner restarts and leaves it out of its first full sync, which replaces the owner's whole
  set (section 3).

The daemon never ends a scope because its parent ended. The children stay live, read
`parent_state: ended`, and their routes drain with `scope_parent_ended` (section 3). The
owner therefore keeps the link current:
- When the head's epoch changes, the owner re-points the run scope's `parent` at the new epoch
  in the same sync that changes the head's epoch, because it builds each sync from the head's
  current record. The run scope keeps its own epoch. This avoids drains only when the run's
  owner also owns the head's scope: then the link stays `linked` and no run route drains, and
  a bind still in flight is refused `scope_changed` and re-opened. When another owner holds the
  head's scope, the re-point comes in a later sync, after the run's routes have drained; the
  link then reads `linked` again for new binds. A route bound before the re-point keeps the old
  parent epoch in its stamp.
- When the head's scope really ends, the owner ends that head's run scopes.

A transient reconnect of a host, carrier or provider closes routes but never changes a scope
epoch: epochs come only from the owner's records (section 2). The run scope and its link stay
as they were, and the carrier re-opens its routes.

A drain does not stop a call already delivered (section 7). The provider answers every
in-flight call with a terminal frame and stops work the run started once `scope.describe`
confirms the scope ended.

**What a provider must do,** in addition to section 7:
1. Declare `call-modes/v1`, list `codemode` only on tools it will serve inside a program, and
   refuse every other tool with `call_mode_unsupported`.
2. Take the agent and the run only from the stamp's `attributes.agent_id` and
   `attributes.run_id`, never from arguments. A project root or session in the bind's identity
   is configuration, never authority. The stamp does not list carriers: the daemon has already
   checked that the opener, stamped as the bind's principal, is the owner or a listed carrier.
3. Grant no more than the agent's own access: the same policy, grants and limits as the agent's
   direct calls, or narrower.
4. Refuse new calls on a route whose stamp reads `parent_state: ended`. The daemon still admits
   binds to a live scope whose parent ended and stamps them that way. A `pending` link is
   unverified and grants nothing.
5. Refuse with a typed reason when serving a call needs state about the agent that the provider
   cannot resolve, never serving from stale state.
6. Record `agent_id` and `run_id` in its audit for every call.

**Finding the agent's own access.** Obligation 3 needs the agent's access context: its project
root, policy and grants. Every provider finds it the same way. It uses the run's stamped
`parent` (owner, ref and epoch, with `parent_state: linked`) only to locate the head's scope.
It then finds its own live route bound under that head scope at the epoch the link names,
requires that route's stamp to carry an `attributes.agent_id` equal to the run's stamp's, and
derives the agent's access from that route. When there is no such live route, `parent_state`
is not `linked`, that route's stamp has no `agent_id`, or the two `agent_id`s differ, the
provider refuses the call with a typed, terminal error (obligation 5). A parent without an
`agent_id` is not a head scope, and a run is never opened under anything else. The parent link
confers nothing by itself: authority comes only from the agent's own route. Both `agent_id`s
can be compared because only an owner in `scope_authority_owners` can set one (section 2). A
run route bound before a re-point names the head's old epoch, so its calls are refused this way
until the carrier re-opens it and gets a stamp naming the new one.

A provider's own tests pin, each failing by name when its rule is removed: a codemode call to a
tool without `codemode` in its `modes` is refused `call_mode_unsupported` with `{tool, mode}`;
an agent named in the arguments never replaces the stamp's `agent_id`; and a route stamped
`parent_state: ended` serves no new call.

## 5. Reading: `scope.describe` and `scope.subscribe`

`scope.describe {owner, ref}` answers:
- `status`: `live`, `ended` (tombstoned since this daemon incarnation) or `not_live`;
- `scope_epoch` for `live` and `ended`;
- `daemon_incarnation`;
- `owner_synced`: whether this owner has synced since this incarnation started;
- `owner_configured`: whether the owner is a module in the supervisor's roster;
- for `live`, the same fields as the stamp.

Describe does not sweep or check the deadline; a past-deadline scope may read `live` until
the next sweep. Its live stamp includes `expires_at_ms` when set. There is no reader
capability gate for the deadline or `attributes.run_id`. Rust stamp readers ignore unknown
top-level fields, and Swift's management reply path preserves extra stamp fields. The
TypeScript client has no describe API or reply decoder. The deadline can therefore be
returned without a new reader-first window. Pre-0.30 readers whose `ScopeAttributes`
reject unknown fields cannot decode such a reply; run writers must wait for the rollout
window in section 8.

`attributes.on_behalf_of_agent` is returned by describe only when the calling module's
registered manifest provides `scope-on-behalf-of/v1`. The daemon strips it for every
other reader, including one declaring only `call-modes/v1`, because older Rust nested
`ScopeAttributes` decoders reject unknown fields. The scope owner's capability does not
grant the reader support. Unregistered client connections are refused `not_registered`
and never receive a stamp. Writers need not wait for legacy readers to upgrade.
The Rust SDK exposes the attribute on both
`RouteBindRequest.scope.attributes` and `ScopeDescribeReply.scope.attributes`.
Swift's management decoder preserves unknown stamp fields; the TypeScript provider has
no typed scope stamp and its client has no describe decoder, so neither needs a new field.

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

`scope.subscribe` (deferred to a later release) is a held request shaped like
`supervisor.spawn_subscribe`, with the same `{daemon_incarnation, seq}` cursor and too-old-cursor
refusal. It sends a snapshot of live scopes, then events:
- `{owner, ref, scope_epoch, created}` when a scope is created;
- `{owner, ref, scope_epoch, ended}` when a scope ends;
- `{owner, ref, scope_epoch, changed}` when its carriers, `delegates`, `agent_id`, `on_behalf_of_agent`, `flow_id`, `run_id` or `parent_state`
  change;
- `{owner, synced, scopes: [...]}` when an owner's first sync of this incarnation is accepted,
  carrying that owner's full live set.
A scope of that owner the reader holds and not in the `synced` set is ended. Before the owner's
`synced` event, a scope merely missing is held, never treated as ended.

### Service history: `scope.served_by`

The daemon advertises `scope-served-by/v1` in HELLO_ACK `subc_capabilities`
and `server.describe`. A registered module can ask about its own scope epoch:

```json
{"op":"scope.served_by","scope_ref":"session-42","scope_epoch":7,"module_id":"aft"}
```

The reply carries `op: "scope.served_by"`, `answer`, and `daemon_incarnation`
(the same incarnation as `scope.describe`). `answer` is an open string:

- `served`: the target acknowledged `route.bind` under exactly this owner, ref
  and epoch. A refused or unacknowledged bind never counts.
- `not_served`: the epoch is tracked and that module has never acknowledged a bind.
- `unknown`: the epoch was never registered, ended, expired, was replaced, or
  has not been re-registered since this daemon started.

Only `served` is affirmative; future answer strings must not grant access.
History survives route closure and same-epoch updates, but ends with the epoch
and is never persisted. Re-registering an epoch after daemon restart starts
with an empty history, not a carried-over `served`.

The owner is implied by the sending module's registration (`reserved:<module>`),
never supplied in the request. Another module querying the same ref and epoch
reads only its own namespace and gets `unknown` if absent there, even if another
owner has a live, served scope. There is no cross-owner existence oracle.
An unregistered connection is refused `not_registered`.

Rust modules use `ModuleHandle::scope_served_by(scope_ref, scope_epoch, module_id)`.
The non-exhaustive `ScopeServedByReply` exposes `ScopeServedByAnswer`, including
`Other(String)` for future values. Without the advertised capability the SDK
returns `ScopeCallError::NotSupported` without sending anything.

## 5a. Refusals and drains

Every refusal the scope feature adds, in one place. "Retryable" means nothing was sent; the
caller may re-open within its own deadline.

| Code | Where | Meaning | Retryable |
|---|---|---|---|
| `scope_not_synced` | `route.open` | the configured owner has not synced since this daemon incarnation | yes |
| `scope_changed` | `route.open` commit | the record changed between admission and commit | yes |
| `scope_not_live` | `route.open` | the owner synced and the ref is not in its set, the owner is not configured, or the live epoch's deadline has passed | no |
| `scope_ended` | `route.open`, admission or commit | the named epoch is not the live one, or the scope ended | no |
| `scope_epoch_required` | `route.open` | the open named no epoch | no |
| `scope_bind_proof_required` | `route.open`, admission and relay re-check | the record has a hash but no proof was supplied | no |
| `scope_bind_proof_mismatch` | `route.open`, admission and relay re-check | proof malformed or digest differs from the stored hash | no |
| `scope_epoch_in_use` | `scope.sync` / `scope.apply`, per record | conditional epoch/proof replacement has routes, in-flight binds or connected presence holders | no |
| `scope_not_carrier` | `route.open` | the opener is not the owner or a carrier, or not targeted at this module | no |
| `target_flow_unsupported` | `route.open`, admission and relay re-check | a flow scope's target provides neither `call-modes/v1` nor `flow-scopes/v1` | no |
| `target_agent_run_unsupported` | `route.open`, admission and relay re-check | a run scope's target provides neither `call-modes/v1` nor `agent-run-scopes/v1`, or the run delegates and the target lacks `call-modes/v1` | no |
| `scope_unsupported` | carrier, before opening | the daemon does not advertise `scopes/v1` | no |
| `not_registered` | `scope.sync`, `scope.apply`, `scope.describe` | the connection has no module registration | no |
| `scope_sync_not_authority` | `scope.sync`, `scope.apply` | the connection fails the owner's authority check | no |
| `scope_sync_required` | `scope.apply` | this connection must first take authority through an accepted full sync | no |
| `scope_sync_stale` | `scope.sync`, `scope.apply` | the generation is not larger than the last accepted | no |
| `invalid_control_body` | whole sync or apply | an empty or duplicate ref, a ref in both apply lists, or an invalid `flow_id` or `run_id` token | no |
| `scope_live_limit_exceeded` | whole sync or apply | an input list or resulting live set exceeds 10,000 scopes | no |
| `scope_attributes_too_large` | whole sync or apply | a record's attributes exceed 4 KiB | no |
| `scope_epoch_regressed` | per upsert in sync or apply | the epoch is lower than the one held | no |
| `scope_attribute_not_permitted` | per upsert in sync or apply | a gated attribute from an owner not in `scope_authority_owners`, or `on_behalf_of_agent` with agent/delegation/flow/run attributes | no |
| `scope_carrier_targets_invalid` | per upsert in sync or apply | a carrier's target list is empty or has more than 16 entries | no |
| `scope_delegates_without_agent` | per upsert in sync or apply | `delegates: true` without `agent_id` | no |
| `scope_run_id_without_agent` | per upsert in sync or apply | `run_id` without `agent_id` | no |
| `scope_run_id_with_flow_id` | per upsert in sync or apply | both `run_id` and `flow_id` | no |
| `scope_run_id_delegates` | older peers' record validation only | legacy refusal for `run_id` with `delegates: true`; retained as a stable code, no longer emitted by this daemon's validation | no |
| `scope_parent_not_permitted` | per upsert in sync or apply | the parent's owner has synced, and the syncing owner is not the parent's owner or in its `child_owners`, or the parent epoch is not live | no |
| `scope_kind_changed` | per upsert in sync or apply | `kind` differs from the held record at the same epoch | no |
| `scope_epoch_ended` | per upsert in sync or apply | the ref@epoch has an owner-ended tombstone | no |
| `scope_expired` | per upsert in sync or apply | the ref@epoch has an expiry tombstone, or a new epoch's deadline is at or before wall now | no |
| `scope_expiry_immutable` | per upsert in sync or apply | the deadline was added, removed or changed on a live epoch | no |
| `scope_expiry_too_far` | per upsert in sync or apply | a new epoch's deadline is more than 24 hours ahead | no |

Only the target-unsupported and other route-open codes above participate in the route-open
retry decision table. Per-record and whole-call scope codes do not; `scope_expired` is never
an admission code. An apply end's `not_live` is a successful entry outcome, not a refusal.

A hold for `scope_not_synced` is bounded by the opener's own deadline and at most 45 s; a stored
approval is kept, not held, as in section 5. There is no separate daemon-side hold.

Which live routes a record change drains:

| Change in a sync, apply or expiry sweep | Routes drained | `route.closed` reason |
|---|---|---|
| scope omitted, explicitly ended, expired, or replaced by a higher epoch | every route under it | `scope_ended` |
| a carrier entry removed | that carrier's routes | `scope_carrier_removed` |
| a module removed from a carrier's `targets` | that carrier's routes to that module | `scope_carrier_removed` |
| `delegates` true to false | every route under it | `scope_delegation_changed` |
| `agent_id` changed | every route under it | `scope_delegation_changed` |
| `on_behalf_of_agent` added, changed or removed | every route under it | `scope_delegation_changed` |
| `flow_id` changed | every route under it | `scope_delegation_changed` |
| `run_id` changed (including added or removed) | every route under it | `scope_delegation_changed` |
| `parent_state` becomes `ended` | every route under it | `scope_parent_ended` |
| anything else (`child_owners`, a carrier or target added, `parent_state` from `pending` to `linked`, an unchanged record) | none | |

A route whose stamp names an older `version` but is not in a drained row stays up: its stamp
still grants no more than the current record does.

## 6. State, locking, restarts and upgrades

State, all in memory:
- in the scope table: records keyed `(owner, ref)`, per-owner authority connection, generation,
  `owner_synced` and the tombstones since this incarnation, with cause `expired` or
  `owner_ended`. All tombstones share the same bounded oldest-first eviction;
- in the forwarding table: each pending bind's and each route's scope tag `(owner, ref,
  scope_epoch, version)`, and the index from a scope to its routes.

For `scope.sync` and `scope.apply`, the control handler resolves workdir paths
before taking the scope write lock. It passes canonical records and per-record
`invalid_workdir` refusals to the table, which performs no filesystem work.
Slow path resolution therefore cannot hold the lock needed by unrelated scoped
route admissions. Refused records retain their previous state while valid
siblings apply, just as for the table's other per-record refusals.

Lock order is scope table, then forwarding table, always. Commit already holds the forwarding
write lock and never takes the scope lock: it compares the pending bind's captured tag with the
record's current `(scope_epoch, version)`, which a sync, apply or expiry sweep publishes into the
forwarding table in the same step that changes the record. Ending or changing a scope: take the scope write lock, update
the record, then take the forwarding write lock and publish the new `(scope_epoch, version)`. Every
pending bind tagged with the old version is then refused at commit (`scope_changed`, or
`scope_ended` if the scope ended). Live routes are drained only as the table in section 5a says, on
every endpoint including superseded endpoints of a swap: a change in the "none" row, such as adding
a carrier, leaves every live route and its in-flight calls alone. A commit before the publish is
covered by the drain rule; a commit after it sees the new version and refuses.

**Restarts and upgrades are the same case for scopes.** The table starts empty, every
`owner_synced` is false, `daemon_incarnation` is new, and readers hold as in section 5. The
The in-place upgrade does not carry routes or pending
binds and gives the new process a new daemon id, so it does not hand over scope state: owners
re-sync and subscribers take a new snapshot, exactly as after a restart. `version` restarts, which
is safe because it is only ever compared within one incarnation. `scope_epoch` does not: the owner
re-sends it, so a stored approval survives a restart when the same session comes back, and dies
when a new session reuses the ref.

Absolute expiry does not restart with the daemon. Owners re-send the same deadline with the
same epoch, and the fresh store refuses it once passed. Tombstones are not persisted, so the
daemon cannot recognise an expired epoch re-sent without its original deadline after a restart
or tombstone eviction. A fresh store accepts an absent or future deadline. Periodic expiry
uses the same wall-clock source as accepted calls and admission; a timer only prompts a sweep.

## 7. What providers must do

- Read identity only from the bind stamp, never from a request payload.
- Act as an agent only when `delegates` is true, `agent_id` matches, and `owner_authorized` is
  true. `agent_id` alone is identity, never
  permission to act.
- A provider opting into `call-modes/v1` reads the mode on every bind, handles all three known
  modes, refuses unknown modes and enforces its own per-tool mode restrictions (section 4a).
  A legacy `agent-run-scopes/v1` provider receives only non-delegating runs, so `run_id` is
  identity, not permission to act as the agent. A declarer may receive a delegating run and
  applies the same `delegates`, `agent_id` and `owner_authorized` checks as for any scope.
  A run's expiry drains routes but cannot recall a call already forwarded, just like any
  other scope end; providers can use the stamped deadline to cap work to the remaining time.
- Bind stored approvals and grants to `(owner, ref, scope_epoch)`, and decide on them as in
  section 5.
- Treat the route's stamp as fixed for the route's life. A change that revokes authority drains
  the route (section 3), so no new call arrives on it.
- **A drain does not stop a call already delivered.** The daemon never reads data-plane request bodies and
  cannot recall a frame it has forwarded; a drained route's in-flight call may still run in the
  module. So stopping work is the provider's job. The operator ruled (triage row T19) that a
  revocation stops running work, not only new calls:
  - a provider that holds a call past its own reply (an approval, a deferred execution) re-checks
    the scope at the point of execution;
  - a provider running work under a scope that outlives the call (AFT's foreground and background
    commands, Cerebellum's open browser, a Broca run) ends it when the scope is revoked, and
    reports it by name (`killed (scope revoked)`, `closed (scope revoked)`, the run sealed
    Interrupted);
  - **the trigger is a route close followed by `scope.describe`, never the close alone.** The
    route GOODBYE a module receives has an empty body and carries no reason, and routes close for
    ordinary reasons too (a module restart, a daemon upgrade, a client reconnect), on which work
    must survive. So when a route stamped with a scope closes, the provider asks `scope.describe`
    for that scope and ends the work only on a definitive answer (section 5, cases 2 and 3). A
    `not_synced` answer, as after a daemon restart, holds and ends nothing. A carrier removed or
    `delegates` turned off leaves the scope live, so its work is not ended by this rule. Work with
    no open route under the scope checks `scope.describe` once per running task per interval
    until `scope.subscribe` ships in stage 7.

## 8. Compatibility

A provider requiring scope identity refuses a bind without the daemon's scope stamp (§5). Scope owners and their authorized carriers—the modules allowed to open routes for an owner—use scopes only when the daemon advertises `scopes/v1`; failure to open a stamped route returns `scope_unsupported`, never an unscoped fallback.

A caller may retry route-open refusals `scope_changed` and `scope_not_synced`. Older SDKs have a closed retry-code set, so an unrecognized code terminates the call rather than retrying without its scope stamp. Scope-change subscriptions (`scope.subscribe`) are not yet implemented.

Before sending expiry, deltas or run identity, owners check HELLO_ACK's advertised module ops for `scope.apply` (§11). That operation shipped with `expires_at_ms` and `run_id` in subc-protocol 0.30.0, so its presence is their support check; older full-sync bodies without deadline or run ID still decode unchanged.

The daemon sends run-scoped binds only to providers declaring `call-modes/v1` or `agent-run-scopes/v1`; a delegating run requires `call-modes/v1`. Before writing run scopes, owners upgrade any reader using subc-protocol versions older than 0.30.0 that can describe the owner's scope references, because those readers reject unknown `attributes.run_id` fields.

Callosum removes `flow-scopes/v1`, `agent-run-scopes/v1`, and `call-modes/v1` declarations from manifests it re-exports: a remote provider's ability to read these bind fields must not claim that the intervening federation relay can read them.

Rust providers upgrade their wire crates before declaring `call-modes/v1`, construct scope stamps with `ScopeStamp::new` and its setters instead of struct literals that must enumerate every new optional wire field, and handle optional `call_mode` in raw `RouteBind` values. TypeScript ignores extra bind fields and has no `scope.describe` reply decoder; Swift preserves describe deadlines in management replies but has no provider-side `route.bind` decoder.

For a provider declaring new and legacy capabilities, an older-daemon bind may omit `call_mode`; the SDK then derives codemode for run scopes, flow mode for flow scopes, and classic mode for plain or unscoped binds (§9). Owners send delegating run records only after deploying a daemon that supports delegating runs.

## 9. Tests

Each fails by name when its rule is removed:
- only the owner or a listed carrier is admitted; `direct` can neither sync nor own;
- an open without `scope_epoch`, the owner's included, is refused `scope_epoch_required`;
- a change of `kind` at the same epoch is refused `scope_kind_changed`; re-sending a tombstoned
  epoch is refused `scope_epoch_ended` or, for expiry tombstones, `scope_expired`;
- a child synced before its parent's owner is accepted as `pending`, becomes `linked` or `ended`
  when the parent's owner syncs, and is never refused for the order; every restart ordering of
  parent and child owners is tested;
- a parent ending drains its children's live routes with `scope_parent_ended`;
- a parent ending never ends its children: a live child whose `parent_state` is `ended` still
  admits binds, stamped `parent_state: ended`, and its routes drain with `scope_parent_ended`:
  `a_parent_ending_never_ends_a_child_which_still_admits_binds_stamped_ended` (`crates/subc-daemon/src/scopes.rs`);
- re-pointing a live child's `parent` at the new epoch in the same sync that replaces its
  parent's epoch keeps the child's epoch, keeps the link `linked` and drains nothing; a
  re-point in a later sync, after the link ended, makes it `linked` again for new binds:
  `a_child_repointed_in_the_same_sync_stays_linked_and_a_later_repoint_relinks_it` (`crates/subc-daemon/src/scopes.rs`);
- adding a carrier leaves another carrier's in-flight call intact, and a stale pending bind is
  refused at commit;
- a swap candidate never takes sync authority, including one that fails and is rolled back, and
  the serving owner's syncs are accepted throughout;
- an open naming a `scope_epoch` other than the live one is refused `scope_ended`; an open before
  the owner's first sync of this incarnation is refused `scope_not_synced` (retryable), and after
  it, for a ref not in the set, `scope_not_live`;
- an apply changes only its named refs, requires this connection's accepted full sync,
  and obeys the shared generation and per-record checks; a reconnect or promoted connection
  cannot take authority through apply;
- an exact-epoch end drains routes and ends child links without ending the children; any other
  end is `not_live`; accepted empty or ineffective applies still advance generation, and a
  later full sync still ends omitted scopes;
- whole-call refusals, including a resulting live-count overflow, change no records, tags,
  routes, generation or expiry state; at the cap an end or accepted-call expiry frees a slot;
- stepping the injected wall clock and sweeping ends scopes at the deadline, even without an
  authority connection, but leaves authority and generation unchanged; no-deadline scopes
  survive and a backwards step never revives a tombstoned epoch;
- accepted calls sweep only their owner's expired scopes; admission refuses a passed deadline
  without mutating state; describe does not sweep; a swept ref appears once in the call's
  `ended`, its old epoch is `scope_expired`, and a higher epoch is `created`;
- deadlines are immutable on live epochs; the exact 24-hour horizon is accepted, beyond it
  (including `u64::MAX`) is refused, and a fresh store refuses a passed deadline;
- `run_id` obeys the attribute-authority and token checks, requires `agent_id`, and cannot
  coexist with `flow_id`; delegating run records are accepted, and changes drain with
  `scope_delegation_changed`;
- both admission and relay re-check allow non-delegating runs with `call-modes/v1` or
  `agent-run-scopes/v1`, require `call-modes/v1` for delegating runs, and allow flows with
  `call-modes/v1` or `flow-scopes/v1`; unsupported targets receive no bind and a failed
  re-check releases the reservation;
- declarers receive codemode for runs, flow for flows and classic for plain or unscoped
  binds; legacy targets retain scope attributes but receive neither `call_mode` nor
  `expires_at_ms`, even with a deadline; the captured relay endpoint, not an earlier
  admission lookup, chooses the fields;
- a declarer's stamp and a live describe reply carry a set deadline; unknown modes decode
  to `Unknown`, and the SDK derives an absent mode from the scope, including on an
  older-daemon bind to a dual-declaring provider;
- the client sends no apply to an unsupported daemon; apply wire goldens round-trip and
  existing sync bodies without expiry or run identity remain compatible;
- the same ref under two owners is two scopes;
- a gated attribute from an owner not in `scope_authority_owners` is refused;
- a scope ended, or changed, between admission and commit refuses the open, the module's other
  routes stay up, and the reserved pair is released;
- removing a carrier and turning off `delegates` each drain the affected routes;
- ending a scope drains its routes on every endpoint, including a superseded one;
- a swap candidate's sync is refused, promotion changes no scope-table state, the promoted
  connection takes authority with its first full sync, and the superseded connection is refused;
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

## 11. Implementation clarifications

The following implementation rules resolve ambiguous wording above; where they differ, this section governs.

- **Where sync, apply and describe run.** `scope.sync`, `scope.apply` and `scope.describe` are
  module control requests sent on the sender's own registered module connection. The owner is that registration's
  `module_id`; nothing in the request body names it. A connection without a registration (`direct`
  and every client connection) is refused `not_registered`, which is how "`direct` cannot own
  scopes" is enforced. `scope.describe` is readable by any registered module, since every provider
  reads it.
- **Authority across a swap is lazy.** Nothing in the table changes at cutover. A connection may
  sync only while the launch nonce it presented at HELLO is the supervisor's recorded spawn nonce
  for its module, compared in constant time. So a swap candidate before cutover, the superseded
  incumbent after it, and a module the supervisor did not spawn are all refused
  `scope_sync_not_authority`. The promoted process's first full sync takes authority and replaces
  the set at any generation; an apply before that sync is `scope_sync_required`.
- **Advertising.** `scopes/v1` appears in both `server.describe` and HELLO_ACK, and HELLO_ACK lists
  `scope.sync`, `scope.apply` and `scope.describe` among the module ops (`scope.apply` since
  subc-protocol 0.30.0).
- **Codes this note did not list.** `scope_live_limit_exceeded` and `scope_attributes_too_large`
  refuse the whole sync. `scope_carrier_targets_invalid` (an empty target list, or more than 16) and
  `scope_delegates_without_agent` refuse one record. A duplicate or empty ref in one sync refuses
  the whole sync as `invalid_control_body`. On `route.open`, `scope_epoch` is optional on the wire
  so that leaving it out is refused by name (`scope_epoch_required`), not as a malformed body.
- **Wire shapes.** Principals use subc-protocol's `Principal` object (`{kind, module_id}`), not
  the `reserved:aft` string used in examples above. A carrier is `{principal, targets?}`, with
  `targets` absent meaning any module. Attributes are the closed struct
  `{agent_id?, delegates, flow_id?, run_id?}` (`flow_id` since subc-protocol 0.29.0,
  `run_id` since 0.30.0). Records may carry optional `expires_at_ms` since 0.30.0;
  omitted `expires_at_ms` and `run_id` decode as unset and are not serialized.
  Record types refuse unknown fields. `ScopeEnd` is `{ref, scope_epoch}` and its result
  adds `outcome: ended | not_live`.
- **Parent links.** A link is checked only when it is new: a new record, a new epoch or a changed
  parent. An unchanged re-sent link keeps its state. A pending link settling to `linked` or `ended`
  bumps the child's `version`, so a bind in flight on the child is refused `scope_changed`; live
  routes still drain only when the link ends.
- **Narrowing a carrier.** Changing a carrier from any module to a target list drains that
  carrier's routes to modules outside the new list. It counts as removing the implicit any-module
  grant, alongside the rows in section 5a.
- **One reason per route.** When one sync changes a scope in several ways, the drains combine and
  each closed route gets one reason. Closing every route beats closing some carriers' routes, and
  among the reasons that close every route the order is: scope ended, then parent ended, then
  delegation changed.
- **Removing a child owner.** Taking an owner out of `child_owners` does not end a child already
  linked under it.
- **Configured owner** means present in the supervisor's module map.
- **Old modules.** A stamped `route.bind` decodes in modules built before scopes, because
  `RouteBind` does not refuse unknown fields. A test decodes the golden stamped bind with the 0.26
  shape, and the TypeScript provider accepts it.
- **Close reasons in older SDKs.** Both SDKs map an unknown `route.closed` reason to
  must-not-reopen (`subc-client-rs` `RouteCloseReason::from_wire`, TypeScript
  `parseRouteCloseReason`), which is right for the four new scope reasons. The Swift client has no
  close-reason decoder.
