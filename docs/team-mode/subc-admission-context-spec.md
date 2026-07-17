# SUBC spec — Admission-Facts Relay + Conformance Corpus (Room-1 share)

Status: DRAFT v5 (v4 folded rounds 1-3; v5 folds the round-4 size-metric pin). The v2 re-gate (bg_7d786e11) verified B2/B3-mechanics/
B5/rollout-token/validation-split CLOSED and returned four blockers,
folded here: (B1) the frozen contract itself contains a latent
stamping-ownership inconsistency — resolution rides Amendment A1 (room
[#74], v7.3) rather than this spec; (B2→§3.1) normative wire schemas;
(B3→§3.2) the carrier-identity config authority; (B4→§4) the pinned
one-shot SDK path. Should-fixes folded: value-relay wording, error
precedence, mandatory org-profile target allowlist. v1's gate (bg_7f71c026) returned NO-GO
with 4 blockers + 1 blocker-class underspecification; v2 restructures
around the central correction: what crosses the wire is NOT the
admission context (the contract forbids that: §0 "not a wire object")
but fed's ADMISSION FACTS — a new, explicitly-wire object; the
admission context of the frozen contract is COMPOSED inside
alfonso-core by serve admission from those facts plus its own §8 store.
Governing text: room1-org-grant-acting-for-contract.md v7.3 @ f02d9b2f
(Amendment A1, room [#74]-[#79]).
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
package, not the context; the stamping-ownership wording this requires
is contract text as of Amendment A1 (v7.3).

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

### 3.1 Normative fact schemas (wire-exact)

Both packages are JSON OBJECTS; top-level `null` is treated as absent
(serde Option collapses it — stated so no implementer distinguishes
explicit null from omission). subc-core never inspects these;
alfonso-core validates them EXACTLY as follows. Common required
fields: `schema` (integer, ==1; unknown schema → reject
`admission_facts_unsupported_schema`), `verified_class` (string:
"member" | "service"; anything else → reject), `peer_static` (string,
lowercase hex, exactly 64 chars), `org` (string ULID). Epoch/version
numerics are JSON integers 0..2^53-1 (reject fractional, negative,
string-typed). Size limit: the byte length of
`serde_json::to_vec(&post_parse_value)` with serde_json's DEFAULT
feature configuration (no preserve_order: object keys re-sort per
serde_json's BTreeMap ordering; default escaping) — pinned to that
exact serializer because "compact JSON" alone does not determine key
ordering or optional escaping, and the metric must be computable
identically by every validator independent of wire bytes (subc's relay
parse-reserializes, so wire-byte length is not stable). Limit ≤ 4096
bytes; reject at 4097. Non-Rust validators implement the same
algorithm: keys sorted lexicographically by UTF-8 bytes, minimal JSON
escaping per serde_json defaults, no whitespace. Depth: the root object is depth 1, every nested
container (object OR array) increments; maximum 3; reject at 4. Both
checks run over the COMPLETE post-parse value BEFORE unknown-field
tolerance applies (an ignored unknown field still counts toward size
and depth; duplicate-key content discarded by last-wins does NOT — it
no longer exists in the post-parse value). Boundary vectors: 4096-byte
accept, 4097-byte reject, depth-3 accept, depth-4 reject, forbidden
field with null value (still forbidden — presence is by key, value
irrelevant), duplicate-key last-wins, and two serializer-sensitivity
vectors: one with non-ASCII/escapable characters and one with
reordered input keys, each sized to cross the 4096 boundary only if
the serializer diverges from the pinned algorithm. Duplicate keys: last-wins per serde
default, harmless given exact-field validation.

MEMBER package (verified_class=="member") additionally REQUIRES:
`account` (ULID), `role` (string), `membership_epoch` (integer),
`device_epoch` (integer — from the A4 the admission verified),
`bundle_version` (integer, diagnostic). FORBIDDEN: `grant_id`,
`grant_ref`, `service_principal_ulid`, `subject` — presence → reject
`admission_facts_forbidden_field`. (Member-path records need no
grant_ref per the contract's lifetime rule — member records check
membership_epoch only; the v2-gate concern that member facts lack
grant_id is resolved by the contract, not a lookup.)

SERVICE package (verified_class=="service") additionally REQUIRES:
`service_principal_ulid` (ULID). FORBIDDEN: `account`, `subject`,
`role`, `membership_epoch`, `grant_id`, `grant_ref` — presence →
reject `admission_facts_forbidden_field` (the shared-pipe rule:
per-turn subject identity is A3 call metadata exclusively).

Unknown fields OUTSIDE the forbidden sets: ignored (additive
evolution). The forbidden sets are part of the schema version: v2's
"unknown fields ignored" and the forbidden-field rejection compose
because forbidden fields are KNOWN names with security meaning, not
unknowns.

### 3.2 Carrier identity: the config authority

New top-level daemon-config field: `admission_facts_carrier_module_id:
Option<String>`. Validated at config load: when set, it MUST name a
configured, enabled module with `reserved: true` — violation is a
config error (fail startup loud). When unset (default; every personal
daemon), NO carrier exists and any admission_facts-bearing route.open
is rejected. The carrier gate compares
Principal::Reserved{module_id} == the configured value by exact string
equality. Spawn attestation alone is NEVER sufficient. Hard-coding
any module name in subc-core is prohibited.

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
- If permitted: relay the SAME JSON VALUE without content inspection
  or mutation (parse-and-reserialize per the shipped body path; byte
  identity is NOT promised and nothing may depend on it) into
  `ModuleControlRequest::RouteBind { ..., admission_facts }`.
- Destination constraint: `admission_facts_targets: ["..."]`,
  MANDATORY whenever the carrier id is set — config load rejects a
  carrier without a non-empty target allowlist, and rejects
  empty-string entries (fail closed; member identity facts must never
  be routable to an arbitrary target by a buggy fed). Unset carrier ⇒
  the constraint is moot. RUNTIME RULE: entries are exact MODULE IDS;
  the check compares `RouteTarget.module_id` by exact string equality
  (target kind and service_id play no part); an out-of-allowlist
  target on a facts-bearing open rejects with
  `admission_facts_target_not_allowed`, positioned AFTER the existing
  target-existence/role/liveness checks and the carrier gate, BEFORE
  reservation/relay — i.e. the last check before relay, sharing the
  carrier gate's position in the pinned precedence.
- ERROR PRECEDENCE (pinned, matches the shipped flow): target
  existence, role, liveness and bind-support checks run BEFORE the
  principal gate, so an unauthorized facts-bearing open toward a
  nonexistent target yields `unknown_module`, not
  `admission_facts_not_permitted`. This is intentional (no
  information-ordering hazard: both errors are same-trust-surface) and
  conformance vectors assert the shipped precedence.

subc-core validates carrier permission and NOTHING else. Content
validation splits three ways (v1 should-fix pinned): serde rejects
structurally unparseable outer JSON at the existing parse site;
subc-core rejects unauthorized carriers; alfonso-core rejects
unsupported schema / malformed fields / semantic violations (including
the §2 gateway-facts-with-subject case).

## 4. Freshness and reconnect (v1's B4 fix)

Route epochs fence HANDLES, not freshness. Pinned consequences:
- Fed MUST NOT use managed cached-route auto-reopen for admitted
  routes, and the SDK path is PINNED, not advisory: fed uses
  subc-client-rs, which gains ONE new one-shot API
  `open_route_with_admission_facts(target, identity, facts)` —
  unmanaged (no cache entry, no auto-reopen, no retry-resend of the
  facts; a transport failure returns the error to fed). Implementation
  pin from the round-3 source check: the API is built OUTSIDE
  ensure_route/open_route_with_retry (one control_call, one
  send_request); "synchronous rejection" on managed APIs means before
  any I/O or first await; the conformance test asserts AT MOST ONE
  route.open frame is emitted per one-shot invocation across
  disconnect, timeout, and retryable-daemon-error paths. The managed
  APIs (`open_route`, `call`, cached reopen) REJECT non-None facts
  synchronously (`admission_facts_requires_oneshot`). Tests prove the
  reconnect and retry paths never re-send a previously admitted
  package. An admitted route that drops is closed permanently; fed
  re-runs its §2 admission (fresh A4/A2/bundle reads) and calls the
  one-shot API again with fresh facts.
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

1. Re-gate this spec (v4).
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
