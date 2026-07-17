# SUBC spec — Admission-Facts Relay + Conformance Corpus (Room-1 share)

Status: DRAFT v2 for re-gate. v1's gate (bg_7f71c026) returned NO-GO
with 4 blockers + 1 blocker-class underspecification; v2 restructures
around the central correction: what crosses the wire is NOT the
admission context (the contract forbids that: §0 "not a wire object")
but fed's ADMISSION FACTS — a new, explicitly-wire object; the
admission context of the frozen contract is COMPOSED inside
alfonso-core by serve admission from those facts plus its own §8 store.
Governing text: room1-org-grant-acting-for-contract.md v7.2 @ e81fb984
(unamended — v2 no longer needs the amendment v1 would have required).
Date: 2026-07-18

## 1. Corrected model (one paragraph)

fed (ck-callosum) verifies transport identity per the contract's §2
member→org algorithm (Noise static, A4 against the fed cloud key, A2
against the account JWKS from its local bundle). The RESULT of that
verification — the admission facts — crosses the subc wire to
alfonso-core as spawn-attested route metadata. Serve admission in
alfonso-core then composes the CONTRACT's admission context: it takes
fed's facts as the transport-identity input, reads ITS OWN §8 snapshot
store for freshness (epochs, grace phase, refusals), and stamps the
context daemon-internally. The contract's sentence "stamped FROM the §8
snapshot store" is satisfied at the only place the store lives; fed
never reads ALF's store (no cross-process read API, no second cache —
v1's B5 dissolves), and the wire object is honestly named a fact
package, not the context (v1's B1 dissolves without amending v7.2).

## 2. Scope split (v1's B2 fix — the load-bearing correction)

- MEMBER-SESSION binds (path (a)): admission facts describe the
  admitted MEMBER-DEVICE session and are per-bind. Fields: peer_static,
  account, org, role, membership_epoch, bundle_version (the fed-local
  bundle version the A2 was verified from), verified_class="member".
- GATEWAY binds (path (b)): admission facts describe only the GATEWAY
  SERVICE identity: peer_static, org, verified_class="service",
  service_principal_ulid. NO subject, NO grant_ref, NO account — a
  gateway bind is a shared pipe serving many subjects, and per-turn
  subject identity arrives EXCLUSIVELY as A3 serve-layer call metadata
  on each send, exactly as the frozen contract places it (§3 path (b)).
  v1's "valid gateway stamp with grant_ref" vector is deleted; its
  replacement vector asserts the OPPOSITE: a gateway fact package
  carrying subject/grant fields is REJECTED by alfonso-core as a
  protocol violation.

## 3. Wire mechanics (v1's B3 fix — the real ingress path)

Admission facts enter at `route.open`, the only ingress that exists:

```
ClientControlRequest::RouteOpen {
  ...existing (target, identity, consumer_identity, consumer_capabilities)...,
  admission_facts: Option<serde_json::Value>,   // opaque to subc-core
}
```

Relay gate in subc-core's handle_route_open, positioned AFTER
route_open_principal validation and BEFORE route reservation/relay:
- If admission_facts is present and the resolved principal is NOT
  Principal::Reserved{module_id} where module_id == the daemon-config
  FED MODULE ID (exact string compare; the fed entry must be
  reserved:true in subc.jsonc — being merely spawn-attested is NOT
  sufficient, closing the v1 should-fix: any supervised module gets
  spawn attestation, only the configured fed module gets this field):
  reject the open with `admission_facts_not_permitted` (protocol
  violation class, loud). Reject-not-strip, scoped to that route.open
  only — a rejected open wedges nothing else.
- If permitted: relay verbatim (content-opaque) into
  `ModuleControlRequest::RouteBind { ..., admission_facts }`.
- Optional destination constraint (config): `admission_facts_targets:
  ["alfonso-core"]` — when set, carrying binds may only target listed
  module ids; others reject. Default unset (org-daemon config sets it).

subc-core validates carrier permission and NOTHING else. Content
validation splits three ways (v1 should-fix pinned): serde rejects
structurally unparseable outer JSON at the existing parse site;
subc-core rejects unauthorized carriers; alfonso-core rejects
unsupported schema / malformed fields / semantic violations (including
the §2 gateway-facts-with-subject case).

## 4. Freshness and reconnect (v1's B4 fix)

Route epochs fence HANDLES, not freshness. Pinned consequences:
- Fed MUST NOT use managed cached-route auto-reopen for admitted
  routes. An admitted route that drops is closed permanently; fed
  re-runs its §2 admission (fresh A4/A2/bundle reads) and issues a NEW
  route.open with fresh facts. The SDK requirement is the negative one
  (exclude these routes from reopen caches); no new SDK callback
  machinery is required for v1 — fed owns its own reopen loop.
- Facts are immutable per binding (re-admission = new bind). Epoch
  fencing then guarantees a stale binding's facts are unreferencable —
  in its handle-fencing role only, not as a freshness mechanism.
- Serve admission composes the context at bind time and re-reads its
  own CURRENT §8 store at every subsequent decision (the contract's
  atomic-decision rule). bundle_version in the facts is diagnostic
  (staleness audit), never a substitute read — same discipline as
  v1's snapshot_version, now on the honest field.

## 5. Rollout (v1 should-fix: the old-core semantic downgrade)

An old subc-core would silently ignore-and-strip the unknown
route.open field (serde default) and admit the route WITHOUT facts —
a semantic downgrade invisible to the exact-version rule. Fix:
`server.describe` capability token `admission_facts_relay_v1`. Fed
REFUSES org-member admission when the daemon lacks the token (fail
closed; personal-mode traffic unaffected). No protocol version bump:
the field is decode-additive on every shipped consumer (source-verified
in the v1 gate: no deny_unknown_fields in session.rs, TS provider
selects known properties), and the capability token carries the
semantic guarantee the version number cannot.

## 6. Consumer contract (ALF's frozen read, unchanged in substance)

alfonso-core receives admission_facts in on_bind metadata. Frozen
guarantees: presence ⇒ the carrier was the daemon-configured fed module
(spawn-attested, exact-id); absence ⇒ not a fed-admitted session (org
operations fail closed); immutable per binding; unknown extra fields
ignored (schema-additive; schema field bumps only via amendment).
Serve admission owns context composition and every freshness read.

## 7. Conformance corpus (unchanged home, repaired completeness bar)

Home `docs/team-mode/conformance/`: CKCRED artifacts + FED A4 vectors +
subc admission-facts vectors, all stable-id, vendored by commit hash,
amendment-governed. v1 should-fix folded — coverage is tracked by
NORMATIVE REQUIREMENT ID, not per-table: the index enumerates each
normative requirement (tables decompose into security-distinct
rows/transitions/boundaries; prose rules like canonical verifier order
and single-freshness-authority get ids too) and maps requirement id →
vector ids covering it, with the uncovered set listed explicitly. Each
vector pins {input, expected decision or error, responsible seat,
path}. Cross-seat runners emit vector id + normalized outcome.
subc-authored vectors: member facts accepted; gateway facts accepted
(service-identity-only); gateway facts with subject/grant → rejected by
alfonso-core; non-fed carrier → admission_facts_not_permitted;
unparseable body → parse reject; absence semantics; immutability
(facts change requires re-bind); old-core capability refusal (fed side).

## 8. Delivery plan

1. Re-gate this v2.
2. subc-core change: RouteOpen field + carrier gate + optional target
   constraint + capability token + tests (including the negative:
   non-fed reserved module carrying facts is rejected).
3. Corpus scaffolding (index schema + subc vectors); CKCRED/FED pins
   land as their specs produce them.
4. Hand ALF §6; their interface hole closes. Fed's spec consumes §3-§5
   (their roster-authority spine supplies the §2 verification inputs).

## 9. Out of scope

Unchanged from v1: wernicke seam, org-daemon deployment topology,
Room-2 machinery, fed roster internals, serve-admission internals.
