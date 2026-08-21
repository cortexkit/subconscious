# Fleet Config Cascade (auto-approval and every future gate)

Status: CO-SIGNED and build-approved (Ufuk, ask_639e782a). Resolver op and
consumer helper are LIVE and integration-proven against each other; parking
and revision_bump push are the next prefrontal slice.

Ufuk's requirement: fleet config gates are settable at
GLOBAL > WORKSPACE > PROJECT > ALFONSO with override at each level, one common
mechanism for every module enforcing any gate (auto-approval is the first
consumer, not the shape). Plexus is a consumer, not the designer.

## Roles

- **prefrontal** owns RESOLUTION, as a second POLICY DOMAIN on the existing
  wake-policy ladder (`crates/prefrontal-core-store/src/wake_policy.rs`, which
  already resolves agent > project > workspace > global with per-field
  most-specific-wins) — approval posture is new policy content, not new
  machinery. Op: `policy.resolve(domain, gate_id, subject, project_root) ->
  {verdict, revision, ttl_ms}` where subject is the registry AGENT_ID (or a
  session id the resolver maps to one): consumers never learn identity
  topology — the resolver owns the session/principal-to-agent mapping just as
  it owns the scope hierarchy. `revision` is a single monotonic policy
  generation across all scopes; any policy write bumps it, so push
  invalidation and cache watermarks are the same number. It holds agent
  identity, already stamps admission facts modules trust (cerebellum's
  `sessionKind`), and queries entorhinal for workspace/project membership, so
  the hierarchy has exactly one home. Working precedent for the whole shape,
  cited as evidence rather than argument: plexus
  `crates/plexus-core/src/github_identity.rs` already fetches prefrontal's
  read-only `agent.github_identity` fail-closed — enforcer-initiated, no
  caller-carried claims.
- **subc** owns TRANSPORT + STALENESS: revision-stamped verdicts, TTL floor,
  push-on-change over the existing push lane for live caches, and ONE shared
  consumer helper (`subc-client-rs` resolve-with-cache) so the fleet gets a
  single staleness behavior instead of five private ones.
- **consumers** (plexus first) pass only what they already hold: gate id,
  subject principal, project root. No ontology import — if a mechanism makes a
  consumer model workspaces, the cascade is in the wrong layer.

## Constraints (settled, argued in pm_3145409b / pm_9c0ea844)

1. **The enforcer initiates the resolution.** A cascade verdict arriving as a
   claim on a request is forgeable by anything that can reach the enforcer; a
   verdict the enforcer fetches is not. Direction over transport.
2. **Fail closed, split by caller shape — DECISION vs FAULT.** A refusal is a
   decision: it will still be true next tick, so it is visible and standing.
   An unreachable resolver is a fault:
   - ATTENDED callers park the action as a durable pending ask that fires when
     a human appears (queues work, does not drop it).
   - UNATTENDED RECURRING work (pollers) treats unreachable as TRANSIENT:
     no park, no suspend, no cursor advance; existing backoff absorbs it.
     A poller that parks per tick manufactures a wall of stale near-identical
     asks nobody will action — the 875-of-877 non-alarm gap-row class that
     trains dismissal. Both arms are fail-closed; only one mints an artifact.
3. **Bounded call, measured number.** The resolve call carries BOTH a p99
   target and a hard timeout (a p99 target is not a timeout). The timeout is
   ENFORCED BY THE SHARED HELPER in subc-client-rs, not by each consumer, so
   five consumers cannot invent five timeout behaviors around one op. Latency
   is measured and published with the op, not promised: expectation is p50
   <5ms daemon-local (wire floor ~0.3ms measured), steady state zero round
   trips under the TTL cache. Dispatch paths sit under supervisor deadlines;
   no unbounded inline dependency.
4. **The push lane must never carry correctness** — stated as a constraint,
   not a description, because it erodes: the moment any consumer skips a fetch
   because a push said "nothing changed", the restart-catch-up-by-construction
   property is silently gone. Pushes only shorten staleness on live caches;
   correctness comes from fetch + revision + TTL. A cache that dies with its
   process catches up because its first post-restart resolve fetches fresh.
5. **Local BLOCK, never local ALLOW.** Enforcement points may keep an operator
   block independent of the cascade (break-glass narrowing, reachable-module
   independent). Same polarity rule as the MCP router (docs/specs/mcp-router.md):
   local narrows, only the cascade widens.

## Wire contract (pinned as BYTES, not prose)

The authority is the producer-real vector file
`prefrontal crates/prefrontal-core-module/tests/fixtures/policy_resolve/contract_vectors.json`
(each committed request is EXECUTED against live dispatch on the producer's
tests; consumers vendor the file under the repin discipline). Summary, not
authority: transport envelope `{method, params}` out / `{result}` back;
subject is UNTAGGED object-key `{agent_id}` | `{session_id}`; the verdict
vocabulary is closed at authoring — rules carry exactly `allow | deny | ask`
(policy.set refuses others), replies additionally serve `deny` (policy-less
closed default) and `deny_unknown_domain` (undeclared-domain marker);
`unknown_session` / `unknown_agent` are typed refusals, not verdicts; reply
`revision` is the CURRENT global generation at resolve time, never the matched
rule's write stamp. The op serves on the resolver's MANAGEMENT SURFACE.

Lesson pinned with the contract, from three integration drifts (subject
encoding, request envelope, route plane) that field-name prose never covered:
PRE-COMMITTING FIELD NAMES IS NOT PRE-COMMITTING THE CONTRACT — encodings,
envelopes, vocabularies, and planes pin only as producer-real bytes, and a
build-local fake must mirror the CONVENTION, not the draft that invented it.

## Named anti-pattern: materializing the cascade into consumer rows

Rejected by its own proposer (plexus) before this design existed: adding
`grantee_principal` to consumer policy rows materializes a view of a hierarchy
the consumer cannot see. Add a 13th Alfonso to a workspace and nobody remembers
the row; remove one and it lingers holding authority. The sharper framing is
OWNERSHIP, not caching: the consumer holds rows whose correctness depends on
state it has no way to observe, so it cannot even detect that they are stale.

## Parking mechanics (prefrontal, confirmed)

Parking fires for ATTENDED callers only; the unattended arm is transient-fault
retry with zero asks (constraint 2, adopted verbatim). Ask identity is a pure
function of `(gate_id, subject_agent_id)`: ONE open ask per gate+subject,
subsequent refused actions queue behind it and never mint siblings — the
dedupe that keeps a slow answer from becoming an ask wall. An answer writes an
ALFONSO-scope policy row: the answer IS policy authoring, so it persists,
bumps the revision, and no per-action re-asks occur. The answer-authored row
is ATTRIBUTED (`authored_by: ask-answer`, ask id in provenance) so a policy
audit distinguishes operator-authored rows from answer-authored ones — the
same absent-never-fabricated discipline as every other field here. No
auto-expiry by default — a parked decision stays until decided, because
auto-expiry re-manufactures the silent drop the parking exists to prevent;
the ask carries `default_decision` semantics per the existing ask contract.

## Policy authoring (prefrontal, settled)

Cascade policy lives as PREFRONTAL STORE ROWS authored over management ops
(`wake.policy_set` precedent) — NOT config files, which carry the
schema-default clobber history, no revision semantics, and are being actively
thinned. Apps and CLI author through ops. Entorhinal stays pure topology
(which workspace owns this project) and stores no policy.

Gate namespace: freeform dotted ids namespaced by consumer module
(`plexus.github_write`), no registration ceremony. An unresolvable or
policy-less gate resolves to the DOMAIN'S DECLARED DEFAULT — closed for
approval domains — so an unknown gate is safe by construction.

## Process

Ufuk reviews the assembled spec before anything is built; the doc routes to
him at co-sign time.
