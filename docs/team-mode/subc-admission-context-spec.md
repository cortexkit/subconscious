# SUBC spec — Admission-Context Surface + Conformance Corpus (Room-1 share)

Status: DRAFT v1 for adversarial gate. Implements the SUBC share of the
Room-1 partition (room [#69]) against the frozen contract
(room1-org-grant-acting-for-contract.md v7.2 @ e81fb984). ALF's outbox
INITIATE read consumes this surface as frozen once this spec passes its
gate (their declared interface hole; the ordering obligation is ours).
Date: 2026-07-18

## 1. Problem shape

The frozen contract §0 defines the admission context as "the
daemon-internal surface (local API between fed admission and the module
layer; not a wire object) carrying {peer_static, account, org, role,
grant_ref, verified_class}, stamped FROM the §8 snapshot store" — plus
Zone-1 target_agent per v7.2 §3.

On the org daemon that "daemon-internal" boundary crosses two supervised
processes: fed (ck-callosum) performs the Noise handshake and artifact
verification (§2 member→org algorithm), while serve admission and the
§8 snapshot store live in alfonso-core. The context therefore has to
cross the subc wire between two modules of one daemon WITHOUT becoming
a caller-claimable wire object. That is exactly the problem subc's
principal machinery already solves for spawn attestation: a module's
claim is validated by the daemon against infrastructure it controls
(spawn nonces), and the RESULT is stamped by the daemon, never relayed
from the claimant.

## 2. Design

### 2.1 Carrier: stamped bind metadata, fed-attested

The admission context rides `route.bind` metadata on the route fed
opens toward alfonso-core for an admitted member session — the same
relay position as `Principal` and `consumer_capabilities` today — with
a new optional field:

```
RouteBind {
  ...existing fields...,
  admission_context: Option<AdmissionContext>,   // wire-opaque to subc-core
}

AdmissionContext {
  schema: u32,                    // 1
  peer_static: String,            // hex X25519, fed-session identity
  account: String,                // account_ulid from A4
  org: String,
  role: String,                   // identity fact per A2; policy resolves org-side
  membership_epoch: u64,
  grant_ref: Option<GrantRef>,    // gateway-path only
  verified_class: String,         // "member" | "service"
  snapshot_version: u64,          // §8 store version the stamp was read at
}

GrantRef { grant_id: String, org: String, account: String, membership_epoch: u64 }
```

target_agent is NOT in this struct: per v7.2 §3 it is stamped by serve
admission (alfonso-core) at turn admission, downstream of this surface.
This surface delivers who was admitted; serve admission decides what
they address. Keeping the two stamps at their authorities prevents this
surface from ever carrying an agent claim fed cannot know.

### 2.2 Trust rule (the load-bearing sentence)

subc-core relays `admission_context` ONLY on binds whose consumer
connection is the spawn-attested `reserved:<fed-module-id>` principal.
A bind from any other consumer carrying the field is REJECTED at relay
(`admission_context_not_permitted`, protocol violation class) — not
stripped-and-forwarded, rejected loudly: a non-fed module attempting to
stamp admission is either a bug or an attack, and both must surface.

This composes three shipped mechanisms and adds no new trust surface:
spawn attestation authenticates WHICH module speaks; the principal
stamp survives relay; and body-opacity means subc-core validates only
the envelope-level permission (who may carry the field), never the
context's content — fed's verification chain (§2 of the contract) is
the content authority, exactly as the contract assigns it.

### 2.3 Consumer contract (ALF's frozen read)

alfonso-core receives `admission_context` in its `on_bind` metadata.
Guarantees this spec freezes for that read:
1. PRESENCE ⇒ the bind traversed a spawn-attested fed module on this
   daemon. Absence ⇒ not a fed-admitted session (local/direct binds:
   personal-mode traffic; alfonso-core treats absence per its own
   policy, fail-closed for org-scoped operations).
2. IMMUTABLE per binding: re-admission after epoch events is a NEW bind
   (fed tears down and re-binds; epoch fencing on the route makes stale
   contexts unreferencable — channel+epoch reuse cannot resurrect one).
3. snapshot_version is the §8 store version fed READ when composing the
   stamp. ALF's gate MUST re-read its own current store on every
   decision (the contract's atomic-decision rule); snapshot_version
   exists for audit and staleness diagnostics, never as a substitute
   read.
4. Unknown extra fields: ignored (schema-additive evolution; schema
   bump only for incompatible change, amendment-governed).

### 2.4 What subc-core does NOT do

No content validation, no §8 store, no epoch checking, no ACL — the
thin-core invariant holds. subc-core's entire contribution is the
permission gate (2.2) and verbatim relay. The org daemon needs zero
subc-core state beyond what exists: spawn nonces and principal stamping
ship today; the change is one optional field + one relay-permission
check + tests.

## 3. Conformance corpus (second deliverable)

Home: `subconscious/docs/team-mode/conformance/` — assembled from the
two authored sources, vendored by commit hash, refreshed only via the
amendment mechanism (fixture diff + room notice):
- CKCRED artifact fixtures (cortexkit-account, stable vector ids).
- FED A4 vectors (subc-federation, same id scheme; fed key domain).
- THIS SPEC adds the admission-context vectors: valid member stamp,
  valid gateway stamp with grant_ref, relay-rejection cases (non-fed
  carrier, malformed schema), absence semantics, and the immutability
  case (context change requires re-bind).

Corpus index: `conformance/index.json` mapping vector id → source repo,
commit, path, and the contract clause it pins. A seat's conformance run
cites vector ids in failure output. The corpus is complete for the
implementation phase when all three sources are pinned and every
normative table in the frozen contract has ≥1 vector referencing it —
the index carries a coverage table naming any uncovered clause.

## 4. Delivery plan

1. Gate this spec (adversarial pass).
2. Wire change in subc-core (field + relay gate + tests) — small,
   protocol-additive (optional field: no version bump needed under the
   v2 exact-version rule since body schemas are opaque; the
   subc-protocol type addition is additive serde).
3. Corpus scaffolding (index + subc-authored vectors); CKCRED/FED
   pins land as their spec phases produce them.
4. Hand ALF the frozen 2.3 read; their interface hole closes.

## 5. Out of scope

Wernicke gateway seam (own lane, later); org-daemon deployment
topology; any Room-2 machinery; fed's roster-authority internals (their
spec); serve-admission internals (ALF's spec).
