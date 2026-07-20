# Room 2 FED Appendix: federation_exposure schema + forwarder gate amendment

Companion to the Room-2 contract (subconscious `9a5dfcde`, frozen r3) for the joint gate.
FED's §8 partition share: forwarder exposure gate (verified∧exposure), org-daemon reconcile
of compiled exposure sets, profile-schema shape for `federation_exposure`. This appendix
pins the implementation shapes against the contract's §3.2/§5/§6.6.

## 1. federation_exposure schema (FED-owned, contract §5)

`federation_exposure` is a **per-member A2 claim** (the member's effective exposure,
compiled org-side from `role_acls[member.role]`). Its shape mirrors fed's existing
`ExposeEntry` (crates/fed-core/src/profile.rs:133-137) so the forwarder gate consumes it
directly:

```rust
// crates/fed-core/src/profile.rs (existing)
pub enum ExposeEntry {
    Tool { module: String, tool: String },
    Operation { module: String, operation: String },
}
```

Wire shape (closed, deny-unknown-fields; each entry is EXACTLY ONE of `tool` or
`operation`, matching the existing profile validation that rejects both-or-neither and
duplicates):

```json
"federation_exposure": [
  { "module": "<module_id>", "tool": "<tool_name>" },
  { "module": "<module_id>", "operation": "<op_name>" }
]
```

Pins:
- FED OWNS this shape. The org-side compiler (CKCRED schema home) produces sets CONFORMING
  to it; a shape change is a fed-schema amendment the compiler must follow (one authority
  for the shape, mirroring taxonomy1's contract-pinning for the class vocabulary).
- `module` is the fed module_id (the `fed:<peer-fp>:<module_id>` namespace's module_id).
- deny-unknown-fields on every entry; an entry with both `tool` and `operation`, or
  neither, or a duplicate, is rejected at parse (fail-closed, same as the static profile
  validation today).
- An ABSENT `federation_exposure` claim (pre-org peer, or org-grant not yet reconciled)
  means "no org-grant exposure" — the forwarder falls back to the static profile `expose`
  entries (§3). An EMPTY array `[]` means "org-grant explicitly exposes nothing" (fail-closed
  — distinct from absent, matching Room-1's absent-vs-empty discipline).

## 2. Forwarder gate amendment (verified ∧ exposure)

The current gate (crates/fed-core/src/catalog.rs:97-116):

```rust
pub fn exposes_tool(&self, module: &str, tool: &str, verified: bool) -> bool {
    verified && self.expose.iter().any(|entry| matches!(entry, ExposeEntry::Tool { .. }))
}
```

`verified` is the TRUST axis (roster effective_verified, Room-1). `self.expose` is today the
STATIC profile entries. The amendment makes the capability axis DYNAMIC:

```rust
// effective exposure = reconciled org-grant federation_exposure (if present) ELSE static
// profile expose (pre-org fallback). Provenance Local < OrgGrant, matching roster
// provenance.
pub fn exposes_tool(&self, module: &str, tool: &str, verified: bool) -> bool {
    verified && self.effective_expose().iter().any(|entry| matches!(..))
}

fn effective_expose(&self) -> &[ExposeEntry] {
    self.org_grant_exposure.as_deref().unwrap_or(&self.expose)
}
```

Pins:
- The two axes compose as a CONJUNCTION at the forwarder gate: a peer must be BOTH verified
  (roster) AND exposed (effective_expose). Either axis failing denies the call
  (`fed_not_exposed`). The exposure axis is orthogonal to ALF's action taxonomy (action
  class) — both check at serve admission with no ordering dependency.
- `org_grant_exposure: Option<Vec<ExposeEntry>>` is the reconciled per-member
  federation_exposure. `Some([])` = org-grant exposes nothing (deny all); `None` = no
  org-grant, fall back to static `expose`.
- The gate reads the reconciled set in-memory (no per-call bundle read) — the reconcile path
  (§3) updates `org_grant_exposure` live.

## 3. Reconciliation (roster-spine path, 0.3 latch-before-durable)

The org daemon reconciles `federation_exposure` into fed's exposure config on the SAME path
as the roster spine (Room-1 §0.3): the per-member A2 claim's `federation_exposure` is parsed
into `org_grant_exposure` and applied to the peer's forwarder state under the 0.3
latch-before-durable ordering (in-memory latch, then durable commit). A role change atomically
changes exposure (the federation_exposure rides the bundle atomic with role + verified +
ceiling). Enforceable WITHOUT a daemon restart (rides the live reconciliation the roster
spine already proves). FED never re-runs the role→exposure compilation — it consumes the
pre-compiled per-member set (the compiler is single-sourced org-side; re-deriving in fed
would be a second compiler that can skew, the one-authority-per-fact violation Room-1
prevents — ALF [#23] confirmed from the consumer side).

Supersession edges (the gate's named attack surface; r4 F4 distinguishes EXPLICIT
WITHDRAWAL from INTERRUPTION — the fail-open fix):
- org-grant ARRIVES (None → Some, a COMPLETE authoritative reconcile): org_grant_exposure
  set; forwarder now gates on it (supersedes static).
- org-grant EXPLICIT WITHDRAWAL (Some → None, a COMPLETE reconcile authoritatively carrying
  grant-absent — the org relationship ENDED, e.g. peer dropped from org): org_grant_exposure
  cleared; forwarder falls back to static `expose` (or denies all if no static). The
  widen-after-durable discipline applies to the EFFECTIVE set (r5 N1 — a withdrawal is NOT
  categorically a shrink): a withdrawal whose static fallback NARROWS or equals the effective
  set (static ⊆ org-grant) latches in-memory immediately under 0.3 latch-before-durable
  (fail-closed shrink); a withdrawal whose static fallback WIDENS the effective set (static ⊃
  org-grant — static exposes tools the org-grant did not) becomes visible only AFTER the
  durable commit (the widening components delay until durable; never widen in-memory before
  durable, which would be fail-open). For a partial overlap, the narrowing components
  (org-grant-only exposures removed) latch immediately and the widening components
  (static-only exposures added) delay until durable commit.
- INTERRUPTION / STALENESS (a FAILED, PARTIAL, or STALE reconcile): changes NOTHING — the
  last durable org_grant_exposure keeps governing. NEVER widen to static on a transient
  fault: a transient fault must not expose MORE than the org last authorized (that would be
  fail-open). The forwarder holds either the last durable org-grant set or None; there is NO
  representable mid-reconcile provenance (no "partially reconciled" state). This is exactly
  the latch-before-durable posture — a reconcile that does not complete authoritatively
  applies no latch, so the in-memory org_grant_exposure is unchanged.
- org-grant + static BOTH present: org-grant governs (provenance Local < OrgGrant).
- org-grant ABSENT (never reconciled) + static present: static governs (pre-org fallback).
- BOTH absent: nothing exposed (fail-closed).
- org-grant EMPTY `[]` + static present: org-grant governs → deny all (empty != absent).

The forwarder shape for F4 + N1: `org_grant_exposure: Option<Vec<ExposeEntry>>` transitions
to `None` ONLY on a complete authoritative reconcile carrying grant-absent (explicit
withdrawal). A failed/partial/stale reconcile leaves `org_grant_exposure` UNCHANGED (the
last durable set governs); `effective_expose()` therefore never widens to static on a
transient fault. The reconcile path must distinguish "the bundle authoritatively says no
grant for this member" (→ None) from "the reconcile did not complete" (→ no change). On an
explicit withdrawal, the in-memory transition respects the widen-after-durable rule (N1):
narrowing components apply at the in-memory latch; widening components (a broader static
fallback) apply only at the durable commit, so a crash between latch and durable commit
never leaves the effective set WIDER than the last durable state.

## 4. Conformance vectors (contract §6.6, FED)

1. org-grant supersedes static: a peer with static `expose` + a reconciled org-grant
   `federation_exposure` gates on the org-grant (a tool in static but not org-grant is
   denied; a tool in org-grant but not static is allowed if verified).
2. live reconcile without restart: changing the org-grant federation_exposure (role change)
   updates the forwarder gate without a daemon restart (0.3 latch-before-durable).
3. verified∧exposure both required: verified+exposed → allowed; verified+not-exposed →
   fed_not_exposed; not-verified+exposed → fed_not_exposed (the verified axis still gates).
4. absent-vs-empty: absent federation_exposure → static fallback; empty `[]` → deny all.
5. withdrawal (narrowing): org-grant withdrawal whose static fallback ⊆ org-grant restores
   the (narrower) static fallback, latched in-memory immediately (fail-closed shrink),
   reconciled live.
6. shape conformance: deny-unknown-fields; both-tool-and-operation rejected; duplicate
   rejected.

### §6.8 Exposure-transition vectors (r5 N1 — the widening-order discipline)

1. withdrawal-to-broader-static delays until durable (N1): org-grant exposes {A}; static
   exposes {A, B} (broader). Explicit withdrawal: B (the widening) is NOT exposed in-memory
   before the durable commit; a crash between the in-memory latch and the durable commit
   leaves the effective set ⊆ the last durable state (never wider). After durable commit, B
   is exposed.
2. withdrawal-to-narrower-static latches immediately: org-grant exposes {A, B}; static
   exposes {A}. Explicit withdrawal: B is removed in-memory immediately (fail-closed shrink);
   the effective set narrows before the durable commit.
3. withdrawal-partial-overlap decomposes: org-grant {A, B}; static {B, C}. Withdrawal: A
   removed immediately (narrowing), C added only after durable commit (widening); B persists
   throughout.
4. interruption never widens: a failed reconcile with a broader static fallback does NOT
   change the effective set (the last durable org-grant governs; no widening on a transient
   fault).
5. explicit-withdrawal-vs-interruption: a complete reconcile carrying grant-absent → None
   (static governs, subject to the widen-after-durable rule above); a failed/partial/stale
   reconcile → no change (last durable set governs).

## 5. Gate attack surfaces (FED seam, per the chair's scope)

- Exposure supersession edges (§3 above): the six arrival/withdrawal/both/neither/empty
  transitions, each fail-closed and live-reconciled.
- The verified∧exposure conjunction: no path admits on one axis alone.
- The pre-compiled guarantee: fed never re-derives from role_acls[] (single-sourced
  compiler org-side); a fed-side re-derivation would be a contract violation.
- Shape ownership: a federation_exposure entry not conforming to ExposeEntry shape is
  rejected at parse (fail-closed), never coerced.

## 6. Implementation note (sequencing)

This is a fed-module change touching crates/fed-core/src/profile.rs (ExposeEntry parse for
the wire shape), crates/fed-core/src/catalog.rs (effective_expose + the gate amendment), and
crates/fed-module reconcile path (org_grant_exposure from the A2 claim). It composes with the
Room-1 roster spine (already shipped) and consumes the org-grant assertion bundle (CKCRED
carriage). No new fed authority — fed stays authority-transport for ceilings; the exposure
gate is the one capability dimension fed enforces, reconciled from the org-grant.
