# subc Federation Design v4 — Confirmation Re-Gate Synthesis

**Intent:** AUDIT (confirmation pass — verify v4's fold of the 13 v3 findings, not a fresh hunt)
**Council:** 6/6 valid responses — Opus 4.8, GPT 5.4 high, GPT 5.5 xhigh, XAI Composer 2.5, Ollama GLM 5.2, Gemini Flash 3.5 high. All six read the cited subc-core source (registry.rs, forwarding.rs, control.rs, supervise.rs, subc-protocol/manifest.rs) and grounded verdicts against real line numbers.
**Question:** Confirm each of the 13 v3 re-gate findings is actually closed by v4; flag incomplete folds, new cross-section contradictions, and quietly-weakened locked decisions; end GO/NO-GO for phase 0 (build P1 `catalog.update` + P2 prefix reservation in subc-core).

---

## Headline

**The v4 folds are substantively correct and source-consistent — but the re-gate fails on document integrity, not on design.** All six members independently confirm that the *authoritative* P1/P2 specifications in §2.6, and the §6.1 at-most-once mechanics, close the findings they claim to close, verified against the real subc-core source. Nine of the thirteen findings are CLOSED unanimously (6/6).

The blocker is a **single stale sentence — line 130 in §4.1** — that re-asserts the *exact* v3 flagship blocker (#1): "removed tools get route-GOODBYE." Every member flagged it; five call #1 PARTIAL on its account, the sixth (GLM) calls #1 CLOSED but still logs the line-130 contradiction. Alongside it, three members flag stale "one connection per peer" phrasing (lines 102/238/267) that contradicts v4's decided per-(peer, module) topology (#11).

**Vote:** 4 NO-GO (GPT 5.4, GPT 5.5, XAI, Gemini) / 2 GO-conditional (Opus, GLM). **The split is nominal.** Both camps agree on the identical facts: (a) the §2.6 primitive spec that phase-0 actually builds from is correct, complete, and implementable; (b) the same small set of stale contradictory lines must be edited. They differ only on whether a doc-consistency defect *gates the code start* or is fixed *alongside* it. Consolidated verdict below resolves this to a **conditional GO**: a ~4-line documentation pass is the gate, and it is trivial.

---

## UNANIMOUS Findings (6/6)

#### #F1: §4.1 line 130 re-asserts the architecturally-impossible tool-granular GOODBYE that #1 killed (NEW CONTRADICTION)
- **Severity**: High
- **Confidence**: Unanimous (6 members)
- **Members Reported**: Opus 4.8, GPT 5.4, GPT 5.5, XAI, GLM, Gemini
- **Issue**: The authoritative P1 spec (§2.6:84-85) correctly re-scopes to "No tool-granular GOODBYE… a call to a since-removed tool… gets a **module-side typed error**," and the changelog:9 says "never a GOODBYE." But **§4.1:130 still reads "removed tools get route-GOODBYE"** — the precise impossible promise v3 finding #1 (unanimous BLOCKER) eliminated, surviving verbatim in the data-flow narrative. The v4 edit to §2.6 was not propagated to §4.1.
- **Evidence**: §4.1:130 vs §2.6:85 + changelog:9. Source-confirmed impossible: `ModuleRouteKey { endpoint, channel }` (forwarding.rs:43-46) and `RouteBinding` (forwarding.rs:49-60) carry **no tool identity**; module-direction GOODBYE is best-effort by design (forwarding.rs:68-93). subc-core cannot deliver a tool-granular GOODBYE.
- **Impact**: A phase-1 implementer reading §4.1 would attempt to build the tool-granular route-GOODBYE that the routing model provably cannot deliver — reviving the exact v3 blocker. The authoritative §2.6 spec is correct, so this is a documentation defect, not an architecture defect — but it re-introduces the flagship blocker's wording into the live doc.
- **Fix Direction**: Edit §4.1:130 to "removed tools get a module-side typed error (no route-GOODBYE; §2.6 P1)," matching §2.6:85. One-clause edit.

#### #F2: §2.6 P1 frozen-field folds close #2 correctly (concurrency/control_ops confirmed load-bearing)
- **Severity**: — (confirmation: CLOSED)
- **Confidence**: Unanimous (6 members)
- **Members Reported**: all 6
- **Issue/Confirmation**: §2.6:86 freezes `module_id`, role kind, `concurrency`, `control_ops` as HELLO-time and rejects any P1 payload changing them (`catalog_update_frozen_field`) — the converged v3 fix, verbatim.
- **Evidence**: All members source-verified the frozen fields are load-bearing: concurrency sizes the credit window (forwarding.rs:18-22 `DEFAULT_MODULE_MANAGED_WINDOW=32` vs `STATELESS_PARALLEL_WINDOW=1024`), read via `manifest_concurrency` at register (control.rs:619), stored on `ModuleConnection` (forwarding.rs:300-304); `control_ops` derived via `effective_module_control_ops` (control.rs:584), stored registry.rs:83, read by the health prober (control.rs:1260). No contradicting statement elsewhere.
- **Impact**: Split-brain (stale flow window / stale prober ops) eliminated by construction.
- **Fix Direction**: None — CLOSED.

#### #F3: §2.6 P2 prefix semantics + honest same-user threat statement close #3 correctly
- **Severity**: — (confirmation: CLOSED)
- **Confidence**: Unanimous (6 members)
- **Members Reported**: all 6
- **Issue/Confirmation**: §2.6:88-91 specifies delimiter-aware `id.starts_with(prefix)` with mandatory `:` boundary (so `fed:` ≠ `fedx:tool`), exact-id precedence over prefix, overlapping owners rejected at config load, boundary matrix required; prefix → **owner** supervised module_id verified against the OWNER's current spawn nonce; and the honest "P2 is NOT a same-user barrier" statement.
- **Evidence**: Source-verified: `reserved_hello_authorized` is an exact-id map returning `true` on miss (supervise.rs:384-395), and the nonce ships via `SUBC_LAUNCH_NONCE_ENV` — same-user readable (supervise.rs:2033). v4 takes the v3-synthesis's *explicitly sanctioned* branch: document P2 as a different-user/accidental-collision barrier under the accepted same-host floor rather than claim a hard wall.
- **Impact**: Squatting-class named and scoped honestly; no overclaim.
- **Fix Direction**: None — CLOSED. (See Weakened-Decisions: this is honest scoping, NOT a regression.)

#### #F4: §6.1 folds (#4/#5/#6/#6a) close the at-most-once holes with mechanisms
- **Severity**: — (confirmation: CLOSED, two minor residues below)
- **Confidence**: Unanimous on #5 and #6 (6/6); strong majority on #4 (5 CLOSED / 1 PARTIAL) and #6a (see #F7)
- **Members Reported**: all 6
- **Issue/Confirmation**:
  - **#5 (6/6 CLOSED)**: `effect_id = (origin_device_pubkey, incarnation_uuid, seq)` (§6.1:197), incarnation minted on db create/loss/restore, stored in that db; serving side keeps per-(pubkey, incarnation) high-water mark and fences seq regression (typed error, never replay). Directly implements the converged incarnation-epoch fix.
  - **#6 (6/6 CLOSED)**: retention co-defined to the origin's piggybacked confirmed-watermark + bounded grace; post-expiry = typed `effect_outcome_expired` refusal, never re-dispatch (§6.1:199). Circularity resolved.
  - **#4 (5 CLOSED / 1 PARTIAL)**: fed-state → CallError mapping table + recovery reconciliation (§6.1:200-206) closes the pre-intent-crash hole — "no intent row on restart → provably `not_sent` tombstone." Test vectors mandated per row (§6.1:206). This is the mechanism, not just prose, that the v3 synthesis demanded.
- **Evidence**: §6.1:197-206; origin's fixed 4-variant taxonomy at consumer.rs:581-593.
- **Impact**: The three §6.1 durability residues are closed as executable phase-0 spec debt.
- **Fix Direction**: None for #5/#6. For #4, see #F6 (GPT 5.5 dissent on the durable tombstone key/API).

#### #F5: Peer topology (#11) matches eviction semantics in §2.5 — but stale "per peer" phrasing survives elsewhere (NEW CONTRADICTION)
- **Severity**: Medium
- **Confidence**: Unanimous that §2.5 is correct (6/6); Majority (3/6) that stale phrasing is a live contradiction worth flagging
- **Members Reported**: all 6 confirm §2.5; GPT 5.4, GPT 5.5, XAI flag the stale lines as PARTIAL; Opus, GLM, Gemini note them but call #11 CLOSED
- **Issue**: §2.5:79 decides "one loopback connection per **(peer, remote module)**, one HELLO each," matching `register_module_connection` eviction on same connection_id (forwarding.rs:280-282). But **§3.1:102, §8:238, and decision-log:267 still say "one connection per peer"** — the decision log (267) is even marked "locked (§2.5, Ufuk)" while contradicting §2.5's actual text. §4.1:129's "per-peer provider HELLO" is loose wording in the same class.
- **Evidence**: §2.5:79 / changelog:12 / phase-1:251 (correct) vs §3.1:102, §4.1:129, §8:238, decision-log:267 (stale "per peer").
- **Impact**: Implementer ambiguity — does one peer get one connection or N? Not architecturally dangerous (authoritative §2.5 is correct and source-grounded), but the decision log contradicts itself.
- **Fix Direction**: Update lines 102, 238, 267 (and tighten 129) to "one loopback connection per (peer, remote module)."

---

## MAJORITY / MINORITY Findings

#### #F6: §6.1 pre-intent-crash row lacks a concrete durable correlation key/API for the tombstone (MINORITY dissent on #4)
- **Severity**: Medium
- **Confidence**: Minority (2 members — GPT 5.5 primary; GPT 5.4 adjacent via #12 rigor)
- **Members Reported**: GPT 5.5 (calls #4 PARTIAL); the other five call #4 CLOSED
- **Issue**: The fed-state → CallError table (§6.1:203) says recovery "emits a durable `not_sent` tombstone the consumer can query," but if no intent/effect row was ever durable, v4 does not specify the durable **correlation key or API** by which recovery emits/queries that tombstone — and `CallError` remains only the 4 variants (consumer.rs:581-593). The origin has no effect_id in the pre-intent window, so what does it query by?
- **Evidence**: §6.1:203-204 vs consumer.rs:581-593. Opus independently noted the same soft spot: the "caller simply re-invokes" parenthetical (203) glosses that the consumer sees `OutcomeUnknown` in both the before-intent and intent-durable windows and must consult the tombstone/reconciliation to distinguish them.
- **Impact**: Phase-0 test vectors for the pre-intent-crash row cannot be fully written until the durable correlation/tombstone key is specified. This is phase-3-consuming spec debt, not a phase-0 primitive (P1/P2) gate.
- **Fix Direction**: Specify the recovery tombstone's durable key and the query API (or the federation-aware wrapper) before writing the §6.1 crash-cut test vectors. Track as phase-0 executable-spec debt per §9:250.

#### #F7: llm-runner "proven" appeal survives in the historical changelog (line 23) — #6a residue (SPLIT)
- **Severity**: Low
- **Confidence**: Split (3 PARTIAL — Opus, GPT 5.5, XAI; 3 CLOSED — GPT 5.4, GLM, Gemini)
- **Members Reported**: all 6 examined
- **Issue**: §6.1:196 + changelog:11 correctly drop the appeal ("standard WAL discipline… stand on their own"). But **line 23 (v2→v3 historical changelog) still reads "borrowing llm-runner's proven intent-log discipline."** The CLOSED camp treats line 23 as non-operative historical text; the PARTIAL camp notes the flagged phrase literally persists.
- **Evidence**: §6.1:196 / changelog:11 (dropped) vs line 23 (retained).
- **Impact**: Cosmetic — no correctness consequence. Historical-section residue only.
- **Fix Direction**: Optional scrub of line 23 for consistency. Not a gate.

#### #F8: §6.4 ClientHello device-identity residue (#12) — deferred phase-4+ (SOLO stricter read)
- **Severity**: Low (for phase 0)
- **Confidence**: 5 CLOSED-deferred / 1 NOT-CLOSED (GPT 5.4)
- **Members Reported**: all 6
- **Issue**: §6.4:220 still only says `ClientHello` "must carry a device identity" and punts the field-vs-message / back-compat shape to phase-4+. GPT 5.4 calls this NOT-CLOSED on the merits (residue unresolved); the other five accept the phase-4+ deferral as appropriate since #12 was always a NOTE explicitly not gating phase 0.
- **Evidence**: §6.4:220, changelog:17; current `ClientHello` carries only `client_nonce` + `role` (auth.rs:24-28).
- **Impact**: None on phase 0. Real transport-design work owed before phase 4.
- **Fix Direction**: Specify field-vs-post-auth-message and asymmetric-key/symmetric-transport relationship before phase 4. Not a phase-0 gate — even GPT 5.4 agrees this is not the gate.

---

## Findings CLOSED unanimously with no residue

- **#7** (harness story): §5.4:180 — first-class `fed:<peer-fingerprint>` class, explicit provider allowlist coordination, config posture (fed-class = untrusted/project-tier like `mcp:*`), "verify against real AFT before phase 2." CLOSED 6/6, deferred-as-designed to the phase-2 gate.
- **#8** (TOFU): §5.3:170-174 — first contact non-routable until OOB code compared; rotation old-key-signed or re-verified; code binds both endpoints' long-term device static keys; residual documented. All four v3 sub-fixes present. CLOSED 6/6.
- **#9** (partition determinism): §6.2:213 — fed-module reaper is authoritative, closes per-peer loopback connections → connection-granular cleanup → deterministic client-direction GOODBYEs; module-direction GOODBYE demoted to best-effort. Source-verified against forwarding.rs:68-93 (module GOODBYE never-close) vs connection cleanup releasing client routes. CLOSED 6/6.
- **#10** (cross-version): §6.5:223 — raw capability docs filtered/translated before typed decode. Source-verified: closed `ProviderRole` enum, unknown tag fails serde decode (manifest.rs:36-37). CLOSED 6/6.
- **#13** (Fork Cat): §8:243 — now "RESOLVED mechanism: P1 `catalog.update` per (peer, module); open: staleness number only." Stale "coarse re-HELLO" gone from Fork Cat (survives correctly-as-withdrawn only at line 21). CLOSED 6/6.

---

## Summary Table

| # (v3) | Finding | v4 Fold Verdict | Confidence | Members |
|---|---|---|---|---|
| 1 | P1 tool-granular GOODBYE impossible | **PARTIAL** (§2.6 closed; §4.1:130 stale) | 5 PARTIAL / 1 CLOSED-w-flag | all 6 |
| 2 | P1 concurrency/control_ops HELLO-time | **CLOSED** | Unanimous | all 6 |
| 3 | P2 prefix semantics + same-user nonce | **CLOSED** | Unanimous | all 6 |
| 4 | §6.1 pre-intent-crash / taxonomy map | **CLOSED** (1 PARTIAL) | 5 CLOSED / 1 PARTIAL | all 6 |
| 5 | effect_id seq cross-restart durability | **CLOSED** | Unanimous | all 6 |
| 6 | dedup-ledger retention circularity | **CLOSED** | Unanimous | all 6 |
| 6a | llm-runner "proven" appeal | **CLOSED/PARTIAL** (line 23 residue) | 3 CLOSED / 3 PARTIAL | all 6 |
| 7 | `fed:` harness provider story | **CLOSED** (phase-2 gate) | Unanimous | all 6 |
| 8 | TOFU first-contact / rotation / binding | **CLOSED** | Unanimous | all 6 |
| 9 | §6.2 partition determinism | **CLOSED** | Unanimous | all 6 |
| 10 | §6.5 closed ProviderRole enum | **CLOSED** | Unanimous | all 6 |
| 11 | topology per-(peer,module) | **CLOSED** (§2.5); stale §3.1/§8/log | 3 CLOSED / 3 PARTIAL | all 6 |
| 12 | §6.4 ClientHello device-identity | **CLOSED-deferred** (1 NOT-CLOSED) | 5 / 1 | all 6 |
| 13 | Fork Cat "coarse re-HELLO" | **CLOSED** | Unanimous | all 6 |

### New contradictions introduced by v4 edits (b-class)
| ID | Contradiction | Sections | Severity |
|---|---|---|---|
| NC-1 | Removed tools "route-GOODBYE" vs "module-side typed error, no GOODBYE" | §4.1:130 vs §2.6:85 + changelog:9 | **High** |
| NC-2 | "one connection per peer" vs "per (peer, remote module)" | §3.1:102, §8:238, log:267 (+ loose §4.1:129) vs §2.5:79 + changelog:12 | Medium |
| NC-3 | llm-runner "proven" appeal | line 23 vs changelog:11 + §6.1:196 | Low |

### Weakened locked decisions (c-class)
**None.** Unanimous across all 6 members. Two items examined and cleared: P2's "not a same-user barrier" (§2.6:91) is honest scoping the v3 synthesis explicitly sanctioned, not a regression of the exact-id-reservation guarantee; P1's "provides-list-only" narrowing is the sanctioned resolution of #1/#2, a tightening not a weakening. The NC-1/NC-2 items are stale contradictory *prose*, not semantic retreats.

---

## Priority Recommendations

### GATE — must edit before phase-0 code (trivial documentation pass, ~4 lines)
1. **NC-1 (#F1):** Fix §4.1:130 — replace "removed tools get route-GOODBYE" with "module-side typed error (no route-GOODBYE; §2.6 P1)." This is the one item every member flagged; leaving it live re-introduces the v3 flagship blocker's wording into the doc.
2. **NC-2 (#F5):** Fix §3.1:102, §8:238, decision-log:267 (and tighten §4.1:129) to "one loopback connection per (peer, remote module)" to match §2.5:79.

### FIX during phase 0 / before the phase that consumes it
3. **#F6 (§6.1 pre-intent tombstone):** Specify the durable correlation key + query API (or federation-aware wrapper) for the pre-intent-crash `not_sent` tombstone before writing the §6.1 crash-cut test vectors. Phase-0 executable-spec debt (feeds phase 3).
4. **NC-3 / #6a (#F7):** Optional — scrub line 23's "proven" appeal for consistency. Cosmetic.

### DEFER (correctly out of phase-0 scope)
5. **#12 (§6.4 ClientHello):** Resolve field-vs-message + key-relationship before phase 4. Even the lone NOT-CLOSED vote (GPT 5.4) agrees this does not gate phase 0.
6. **#7 (harness):** Verify against real AFT before phase 2, as the doc already gates.

---

## Consolidated Verdict: **GO — conditional on a ~4-line documentation pass (NC-1 + NC-2)**

The 4 NO-GO / 2 GO split is **nominal, not substantive**. Every member — both camps — independently confirms the same two load-bearing facts:

1. **The artifact phase 0 actually builds is correct.** The §2.6 P1 (`catalog.update`: provides-list-only + frozen-field rejection) and P2 (prefix reservation: delimiter semantics + boundary matrix + owner-module nonce mapping + honest same-user threat model) specifications are complete, implementable, and **source-verified** against `reserved_hello_authorized`'s true-on-miss exact lookup, the env-borne launch nonce, the tool-less `RouteBinding`, and the concurrency/control_ops HELLO-time capture points. Building P1/P2 from §2.6 would NOT bake incorrect registry/forwarding invariants. Findings #2 and #3 (the two unanimous phase-0 blockers) are fully CLOSED.

2. **The same small set of stale contradictory lines must be edited.** NC-1 (§4.1:130) and NC-2 (§3.1/§8/log topology) are documentation-consistency defects in *downstream narrative sections* — the authoritative §2.5/§2.6 body is correct. They do not touch the P1/P2 implementation spec.

The NO-GO camp gates the code start on the doc fix (leaving the flagship-blocker's exact wording live is a real hazard for a phase-1 implementer); the GO camp fixes it alongside the code. Athena resolves this to a **conditional GO**: because the gate is a mechanical ~4-line edit that touches no design decision and no source invariant, it should land as a single doc pass **before** the first subc-core commit — satisfying the NO-GO camp's legitimate concern at near-zero cost — after which the phase-0 verdict is an unambiguous **GO**. The §6.1 pre-intent tombstone key (#F6) is executable-spec debt to close before phase 3's at-most-once build, not a phase-0 primitive gate.

**Bottom line: The architecture and the phase-0 primitive specs are sound and confirmed. Edit lines 102, 129, 130, 238, 267 (and optionally 23) to remove the stale contradictions, then start building P1 + P2. GO, gated on that one doc pass.**
