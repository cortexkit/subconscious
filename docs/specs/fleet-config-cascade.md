# Fleet Config Cascade (auto-approval and every future gate)

Status: DRAFT — subc + plexus halves settled; prefrontal half open (resolver
op shape, policy authoring surface, parking confirmation). Co-signed when ALF's
sections land.

Ufuk's requirement: fleet config gates are settable at
GLOBAL > WORKSPACE > PROJECT > ALFONSO with override at each level, one common
mechanism for every module enforcing any gate (auto-approval is the first
consumer, not the shape). Plexus is a consumer, not the designer.

## Roles

- **prefrontal** owns RESOLUTION: a `policy.resolve` management op —
  `(gate_id, subject_principal, project_root) -> verdict + revision + ttl_ms`.
  It holds agent identity, already stamps admission facts modules trust
  (cerebellum's `sessionKind`), and queries entorhinal for workspace/project
  membership, so the hierarchy has exactly one home. Working precedent for the
  whole shape, cited as evidence rather than argument: plexus
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
   target and a hard timeout (a p99 target is not a timeout). Latency is
   measured and published with the op, not promised: expectation is p50 <5ms
   daemon-local (wire floor ~0.3ms measured), steady state zero round trips
   under the TTL cache. Dispatch paths sit under supervisor deadlines; no
   unbounded inline dependency.
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

## Named anti-pattern: materializing the cascade into consumer rows

Rejected by its own proposer (plexus) before this design existed: adding
`grantee_principal` to consumer policy rows materializes a view of a hierarchy
the consumer cannot see. Add a 13th Alfonso to a workspace and nobody remembers
the row; remove one and it lingers holding authority. The sharper framing is
OWNERSHIP, not caching: the consumer holds rows whose correctness depends on
state it has no way to observe, so it cannot even detect that they are stale.

## Open (prefrontal)

- `policy.resolve` op shape + gate_id namespace; where cascade policy is
  AUTHORED (scope-level config files vs store rows; workspace scope likely
  entorhinal-adjacent) — authoring is decision-plane.
- Parking mechanics for the attended arm (whose ask, what dedupe, what expiry).
- Confirmation of the decision-vs-fault split (constraint 2) since the parking
  machinery is prefrontal's.
