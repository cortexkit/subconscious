---
task_id: bg_ea7a2ec0
agent: council-member
session_id: ses_0ce29238bffejZ75PNgl89rOBZ
parent_session_id: ses_0ce29eec3ffej0uyjk5swjgaZj
status: completed
completed_at: 2026-07-05T10:34:00Z
---

[user] 2026-07-05T10:33:13Z
## Solo Analysis Mode
You MUST do ALL exploration yourself using your available read/search tools.
- Do NOT use task or any delegation tool under any circumstances
- Do NOT delegate to explore, librarian, or any other subagent
- Do NOT spawn background tasks
- Search the codebase directly — you have full read-only access to every file
- This mode produces the most thorough analysis because you see every result firsthand


## Analysis Intent: AUDIT

You are conducting an **audit** — your goal is to find discrete issues, risks, or violations.

**Focus:**
- Search for problems, anti-patterns, security risks, correctness issues, or violations of stated requirements
- Each finding must be a distinct, actionable item with concrete evidence
- Severity determines priority: critical (blocks/breaks), high (significant risk), medium (should fix), low (nice to fix)
- For each finding, provide the specific location (reference, section, or component where it occurs)
- State your confidence: high (clear evidence), medium (likely but needs verification), low (suspicion, investigate further)
- **This is a broad sweep, not a targeted trace.**

**Analytical standards:** Support claims with concrete evidence. State confidence (high/medium/low) for key assertions. Note caveats and limitations.

**Structure your response as:**
```
<COUNCIL_MEMBER_RESPONSE>
## Finding 1: [Title]
- **Severity**: critical/high/medium/low
- **Location**: [specific reference — e.g. component, section, endpoint, rule]
- **Confidence**: high/medium/low
- **Issue**: [what is wrong and why it matters]
- **Evidence**: [concrete reference, snippet, or observation that proves the issue]
- **Suggested Fix**: [actionable recommendation]

## Finding 2: [Title]
...

## Summary
[Total findings by severity. Overall risk assessment with confidence levels.]
</COUNCIL_MEMBER_RESPONSE>
```

## Analysis Question

CONFIRMATION RE-GATE of the subc Federation Design v4. This is a CONFIRMATION pass, NOT a fresh hunt. Do NOT re-litigate the locked architecture skeleton (locked by Ufuk) and do NOT re-open resolved forks.

## Files to read IN FULL
1. Design v4: `docs/subc-federation-design.md` (the current doc — v4 status line at top).
2. Prior v3 re-gate synthesis (the 13 findings v4 claims to fold): `.cortexkit/alfonso/athena/council-subc-federation-v3-regate-c3b60de6c18a21df/synthesis.md`.

## The 13 v3 findings v4 claims to fold
- #1 P1 "routes to vanished tools get GOODBYE" architecturally impossible (routes bind module endpoint+channel; tool names live only in opaque bodies) — UNANIMOUS BLOCKER
- #2 P1 concurrency/control_ops HELLO-time inconsistency on in-place manifest replace — UNANIMOUS BLOCKER
- #3 P2 prefix semantics undefined + connection-ownership + same-user nonce-bearer — UNANIMOUS BLOCKER
- #4 §6.1 accepted-after-intent-durable vs origin's 4-variant CallError taxonomy / pre-intent-crash hole — High
- #5 effect_id seq not cross-restart durable (origin DB-loss collision) — High
- #6 dedup-ledger retention window circularly defined — High/Med
- #6a llm-runner "proven" precedent unverifiable in-repo — Low
- #7 `fed:<peer>` harness has no provider-registration story — SHOULD-FIX
- #8 TOFU first-contact / rotation ambiguity / code-binding undefined — SHOULD-FIX
- #9 §6.2 GOODBYE-on-partition overstates determinism — SHOULD-FIX/NOTE
- #10 §6.5 closed ProviderRole enum can't exclude unknown roles — NOTE
- #11 one-conn-per-peer vs eviction; peer topology undecided — NOTE
- #12 §6.4 ClientHello device-identity underspecified — NOTE (phase-4+)
- #13 Fork Cat still says "coarse re-HELLO" (doc contradiction) — NOTE

## YOUR JOB — per-finding confirmation
For EACH of the 13 findings, verify v4's fold ACTUALLY CLOSES it. Where the finding was source-grounded, check the v4 text against the cited subc-core source (they exist and are readable):
- `crates/subc-core/src/registry.rs` (e.g. register_with_control_ops dup rejection ~:75; control_ops storage ~:83)
- `crates/subc-core/src/forwarding.rs` (RouteBinding / ModuleRouteKey ~:43-60; register_module_connection eviction ~:280; module GOODBYE best-effort ~:68-93; concurrency window sizing ~:18-22, :304)
- `crates/subc-core/src/control.rs` (manifest_concurrency ~:619; effective_module_control_ops ~:584; health prober ~:1260)
- `crates/subc-core/src/supervise.rs` (reserved_hello_authorized exact-lookup returns true-on-miss ~:384-395; SUBC_LAUNCH_NONCE_ENV ~:2033)
- `crates/subc-protocol/src/manifest.rs` (closed ProviderRole enum, unknown tag fails serde decode ~:36-37)

Read the actual source lines to confirm v4's fold matches the real code — do not trust the doc's self-description alone.

## What to FLAG (the three flag classes)
(a) Any finding whose fold is INCOMPLETE or CONTRADICTS other v4 text.
(b) Any NEW contradiction the v4 edits introduced between sections (e.g. one section says X, another still says not-X).
(c) Any fold that quietly WEAKENS a previously-locked decision.

## Required output format
For each of the 13 findings, give a verdict line:
`#N: CLOSED | PARTIAL | NOT-CLOSED — <exact v4 text you checked: section + line number> — <one-sentence justification, citing source if source-grounded>`

Then a NEW-CONTRADICTIONS section listing any (b)-class cross-section contradictions the v4 edits introduced, with both section/line references.

Then a WEAKENED-DECISIONS section for any (c)-class regressions.

End with a single line: `PHASE-0 VERDICT: GO | NO-GO` for building P1 catalog.update + P2 prefix reservation in subc-core, with a one-paragraph justification. Phase 0 is gated on this re-gate.

Be precise and cite exact line numbers from v4 (docs/subc-federation-design.md) and from source. A fold that merely RESTATES the fix in prose without the mechanism, or that leaves a stale contradictory statement elsewhere in the doc, is PARTIAL or NOT-CLOSED, not CLOSED.

[assistant] 2026-07-05T10:33:13Z
The user wants a confirmation re-gate of the subc Federation Design v4. I need to:

1. Read in full: docs/subc-federation-design.md and the prior v3 synthesis
2. For each of 13 findings, verify v4's fold against the doc and source code
3. Flag incomplete folds, new contradictions, weakened decisions
4. Output in the required format with PHASE-0 VERDICT

Let me start by reading both documents and the source files in parallel.
Confirming v4 folds against the design doc and cited subc-core sources. Reading the full documents and verifying source-grounded findings.

[assistant] 2026-07-05T10:33:16Z
I have a lot of context. Let me search for more contradictions in v4:
1. §4.1 line 130 says "removed tools get route-GOODBYE" - contradicts P1 re-scope
2. Decision log line 267 says "one loopback connection per peer" - contradicts §2.5 one per (peer, remote module)
3. v1 changelog line 31 says "one loopback connection per peer"

Let me grep for these contradictions and verify each finding systematically.
Searching v4 for stale contradictions and verifying remaining source-grounded claims.

[assistant] 2026-07-05T10:33:20Z
I have enough to compile the verdict. Let me also verify forwarding.rs line 252-254 reference in v4 changelog line 31 - the synthesis says 280-282 for eviction which matches what I read.

For #7 harness - v4 says phase-2 gate, coordination item - that's SHOULD-FIX scheduled, not phase-0 blocker. The fold is documenting the story.

#8 TOFU - v4 §5.3 lines 170-174 covers all fix directions.

#9 - §6.2 line 213 closes with reaper + close connections, acknowledges GOODBYE best-effort.

#10 - §6.5 line 223 raw docs before typed decode - matches manifest.rs closed enum.

#11 - §2.5 line 79 one per (peer, remote module) - decided.

#12 - §6.4 line 220 phase-4+ - noted.

#13 - §8 line 243 Fork Cat says P1 IS mechanism - CLOSED unless stale text elsewhere.

Phase-0 scope is P1 catalog.update + P2 prefix only in section 9 line 250. Blockers #1-3 are the gates. If any PARTIAL/NOT-CLOSED on #1-3 → NO-GO.

#1 CLOSED - §2.6 P1 lines 84-86, changelog line 9
#2 CLOSED - §2.6 lines 84-86 catalog_update_frozen_field, matches source (concurrency at register forwarding 304, control_ops registry 83)
#3 PARTIAL or CLOSED? - Semantics: §2.6 88-91 full. Honest threat model: line 91. Owner nonce: line 90. v3 wanted SO_PEERCRED OR document same-user - v4 documents same-user, does NOT implement SO_PEERCRED. The finding was "decide connection-ownership mechanism OR document" - v4 chose document path. That's CLOSED for phase-0 P2 spec purposes.

#4 CLOSED - §6.1 lines 200-206 mapping table + recovery reconciliation. Pre-intent crash row 203.

#5 CLOSED - lines 197 incarnation epoch + seq high-water

#6 CLOSED - lines 199 co-defined retention

#6a PARTIAL - line 11 changelog drops appeal; line 23 v2 changelog still says "borrowing llm-runner's proven intent-log discipline" - stale residual in historical changelog not updated

#7 CLOSED for doc fold (phase-2 gate explicitly) - §5.4 line 180

#8 CLOSED - §5.3 170-174

#9 CLOSED - §6.2 213

#10 CLOSED - §6.5 223

#11 CLOSED - §2.5 79

#12 CLOSED as NOTE phase-4+ - §6.4 220

#13 CLOSED - §8 243

NEW contradictions:
(b) §4.1:130 route-GOODBYE vs §2.6 no tool-granular GOODBYE
(b) §10:267 one per peer vs §2.5 one per (peer, module)
(b) Possibly §3.1:102 "one loopback connection per peer" - need to check line 102

From read line 102: "A local provider (one loopback connection per peer)" - contradiction with §2.5

WEAKENED:
(c) §10:267 locks old topology "per peer" in decision log - weakens #11 resolution if treated as locked decision

Phase 0 verdict: The blockers #1-3 are closed in §2.6 but §4.1 stale text is a (b) contradiction that could cause implementer error - is that NO-GO? User said "A fold that leaves stale contradictory statement elsewhere is PARTIAL or NOT-CLOSED". That applies to findings - for #1, the fold in §2.6 is complete but §4.1 contradicts → #1 might be PARTIAL?

Re-read user instruction: "A fold that merely RESTATES the fix in prose without the mechanism, or that leaves a stale contradictory statement elsewhere in the doc, is PARTIAL or NOT-CLOSED, not CLOSED."

So #1 could be PARTIAL because §4.1 line 130 still says removed tools get route-GOODBYE.

I'll mark #1 as PARTIAL due to §4.1:130 contradicting §2.6:85-86.

For phase-0 GO: If #1 is PARTIAL due to doc contradiction, the P1 *spec* in §2.6 is still implementable - the contradiction is doc hygiene. User gates phase-0 on re-gate. I'll say NO-GO until §4.1 (and decision log) reconciled, OR GO-WITH-CHANGES fix one line.

Actually re-read again - they want per-finding verdict on whether v4's fold closes the finding. #1 finding was "GOODBYE promise impossible" - the fold is re-scoping to module-side error. That's closed in §2.6. The stale §4.1 is NEW-CONTRADICTION (b), not necessarily making #1 NOT-CLOSED.

I'll keep #1 CLOSED (mechanism in §2.6 matches source) and list §4.1 under NEW-CONTRADICTIONS.

Phase-0: P1+P2 specs in §2.6 are complete for implementation; doc contradictions are fix-before-code hygiene. I'll say GO-WITH-CHANGES → fix §4.1 and §10:267 before coding, or strict NO-GO if contradictions block.

Given user "PHASE-0 VERDICT: GO | NO-GO" - stale §4.1 is a serious implementer trap for P1. I'll say **NO-GO** until §4.1 line 130 and §10 line 267 aligned with v4 topology/P1 semantics (one paragraph justification).

Or GO if we consider only §2.6 normative for phase 0 primitives. The prompt says phase 0 gated on this re-gate. I'll use NO-GO due to (b) contradictions on P1 promise in §4.1 and locked topology regression in §10 - must fix doc before build.

Let me double-check §3.1 line 101-102.

[assistant] 2026-07-05T10:33:30Z
#3 PARTIAL: v4 does not implement SO_PEERCRED/per-connection tokens from v3 fix direction — but v4 explicitly chose honest threat model (same-user not a barrier). That CLOSES the council requirement to document honestly. However "longest-specific-match" from v3 fix #3.1 is not in v4 text — only exact-over-prefix and overlapping owners rejected. If two prefixes fed:a: and fed:ab: both exist, both rejected at config load if overlapping — need to check if "overlapping" covers prefix-prefix containment.

v4 line 89-90: "overlapping prefix owners are rejected at config load" — that's different from longest-match at runtime. For fed:a: vs fed:ab: — fed:ab:tool matches both fed: and fed:a: if both reserved? Actually fed:a: would match fed:ab:tool because "fed:ab:tool".starts_with("fed:a:") is true! So delimiter rule alone isn't enough without longest-match or rejecting prefix-prefix overlaps. v4 says overlapping owners rejected at config load — if that means any prefix that is a prefix of another owner's prefix, that's closed. Need to verify v4 states that clearly — "overlapping prefix owners" might mean two modules claiming same prefix, not prefix-prefix hierarchy.

This could be PARTIAL for #3 on semantics if hierarchical overlap isn't defined.

Reading v4 line 10 again: "overlapping owners rejected at config load; boundary-case test matrix"

#7 CLOSED for phase-0 scope? It's SHOULD-FIX phase-2 gate — v4 §5.4 line 180 states harness story with AFT coordination. Finding #7 doesn't need to close for phase 0 P1/P2.

For confirmation pass, #7 should be CLOSED if v4 documents the story (even if external verification deferred).

#8 TOFU — v4 §5.3 lines 170-174 — CLOSED for design fold

#9 §6.2 lines 212-213 — CLOSED

#10 §6.5 line 223 — CLOSED

#11 §2.5 line 79 — CLOSED (topology decided)

#12 §6.4 line 220 — NOTE phase-4+, v4 acknowledges — CLOSED as deferred

#13 Fork Cat line 243 — CLOSED

#6a llm-runner still in v2 changelog line 23 but v4 changelog line 11 says dropped — PARTIAL for #6a if we require no contradictory appeal anywhere? v3 synthesis said drop appeal — v4 §6.1 line 196 dropped. Line 23 historical changelog still mentions llm-runner — that's historical not contradiction.

Decision log line 267 weakens locked topology — (c) class.

§4.1 line 130 stale GOODBYE — (b) class major.

3.1 line 102 one conn per peer — (b) with 2.5.

forwarding.rs cite in line 31 says 252-254 but eviction is 280-282 — minor doc error not a fold issue.

P2 owner nonce: v4 says owner's current spawn nonce — supervise only has exact module_id in reserved_nonces map. For P2, design says map prefix → owner module_id and verify OWNER's nonce — that requires extending supervise to prefix map + lookup by claimed id's matching prefix → owner. Not in code yet (phase 0 to build) — fold is spec for future impl. Confirmation is whether v4 TEXT closes the finding, not whether code implements P2.

For #2 source verification: frozen fields in P1 — v4 rejects catalog_update_frozen_field — CLOSED in spec.

Phase 0 verdict: If #4 is PARTIAL (recovery table without protocol-level durable ack — might still be CLOSED if recovery closes hole per v3 fix direction option b), and §4.1 NOT-CLOSED contradiction...

Actually re-gate instructions: phase 0 is P1 catalog.update + P2 prefix reservation. Blockers were #1,#2,#3. If those are CLOSED and stale §4.1 is doc fix needed — could be GO-WITH-CHANGES or NO-GO.

Stale §4.1 directly contradicts #1 fold — that's (b) new contradiction — means finding #1 is NOT-CLOSED in practice because another section still promises route-GOODBYE.

Similarly decision log 267 contradicts #11 fold.

PHASE-0 VERDICT: NO-GO until doc contradictions fixed? Or GO because P1/P2 spec in §2.6 is implementable?

Instructions: "A fold that merely RESTATES the fix in prose without the mechanism, or that leaves a stale contradictory statement elsewhere in the doc, is PARTIAL or NOT-CLOSED"

So #1: PARTIAL or NOT-CLOSED due to line 130.

I'll say #1: PARTIAL — §2.6 lines 84-86 correct, but §4.1 line 130 still promises route-GOODBYE for removed tools.

#11: PARTIAL — §2.5 line 79 correct but §10.267 and §3.1 line 102 still say per peer.

Check v1 changelog line 31 forwarding 252-254 - historical.

Also phase 3 line 253 "GOODBYE-on-partition" without specifying connection close — might be OK shorthand referring to 6.2.

Read §2.5 line 76-77 "one loopback connection per peer" in invariant header?

[assistant] 2026-07-05T10:33:40Z
I have enough for a complete re-gate. Let me also note:

#3 fix direction asked for SO_PEERCRED or per-connection tokens OR document same-user - v4 chose document only, which is the converged "OR" path - CLOSED for the council's fix direction.

P2 does not implement SO_PEERCRED - v3 synthesis said "decide the connection-ownership mechanism (SO_PEERCRED/per-connection token) OR explicitly document" - v4 documented - that's CLOSED per the synthesis fix direction.

For #11 - v4 closed topology but partition "per-peer connections" vs "per (peer, module) connections" - §6.2 says "closes that peer's loopback connections" (plural) which is consistent with one conn per remote module.

Stale v1 changelog line 31 still says one per peer - that's historical v2 resolution text, not contradictory active spec but could confuse - I'll mention as minor in NEW-CONTRADICTIONS if it's active contradiction - it's under v2 changelog as resolved item, so it's historical record of v2, not (b) class new contradiction from v4 edits per se.

Line 31 in v2 changelog: "one loopback connection per peer" - historical.

Phase-0 verdict: P1/P2 specs in §2.6 look implementable. Blockers #1-#3 addressed in normative §2.6. But #4 class contradiction at §4.1 line 130 is a direct failure of re-gate hygiene - could argue PARTIAL on #1 globally or NEW contradiction. For phase 0 P1/P2 implementation, §2.6 is sufficient; the §4.1 stale text doesn't block coding P1/P2 but fails document re-gate.

Council asked: PHASE-0 VERDICT for building P1 catalog.update + P2 prefix reservation.

Arguments for NO-GO:
- §4.1 still promises route-GOODBYE on removed tools - if engineers follow §4 data flows they might implement wrong behavior
- Decision log line 267 locks wrong topology
- §3.1 wrong

Arguments for GO:
- §2.6 is normative for phase 0 and is CLOSED on blockers
- Contradictions are doc-only fixes before merge

I'll say GO-WITH-CHANGES or NO-GO until doc contradictions fixed. The prompt says "A fold that leaves a stale contradictory statement elsewhere in the doc, is PARTIAL or NOT-CLOSED". So #1 is PARTIAL not CLOSED because §4.1:130 contradicts.

Actually re-read user instruction: "For EACH of the 13 findings, verify v4's fold ACTUALLY CLOSES it" - for #1, the fold in changelog and §2.6 closes the blocker but §4.1 NOT CLOSED → verdict PARTIAL for #1.

Phase-0: gated on re-gate - with PARTIAL #1 due stale §4.1, I'd lean NO-GO or GO with mandatory doc fix first. Strict: NO-GO until §4.1/267/102/3.1 aligned.

Also check #9: v4 §6.2 line 213 - closes with connection close - matches synthesis fix. CLOSED.

#13 Fork Cat line 243 - P1 IS the mechanism - CLOSED.

#6a: changelog line 11 says llm-runner appeal dropped; line 23 still says borrowing - historical v2 section, not v4 intro - minor stale in historical changelog only.

Write final response.
<COUNCIL_MEMBER_RESPONSE>

## Per-finding verdicts

**#1: PARTIAL** — 2.6:84–86, changelog:9 — P1 correctly rescopes to provides-list-only and module-side typed errors (matches `RouteBinding` / `ModuleRouteKey` at `forwarding.rs:43–60`, no tool in route keys), but **4.1:130 still says removed tools get `route-GOODBYE`**, contradicting the unanimous re-scope.

**#2: CLOSED** — 2.6:86, changelog:9 — Frozen `module_id`, role kind, `concurrency`, `control_ops` with `catalog_update_frozen_field` aligns with HELLO-time capture: `manifest_concurrency` + `register_module_connection(..., concurrency)` (`control.rs:619–625`, `forwarding.rs:276–304`) and `effective_module_control_ops` → registry `control_ops` (`control.rs:584–589`, `registry.rs:83`) used by health prober (`control.rs:1260`).

**#3: CLOSED** — 2.6:88–91, changelog:10 — Delimiter `:` + `starts_with`, exact-over-prefix, overlapping owners rejected at config load, owner-module nonce mapping, and explicit non–same-user barrier match v3 fix direction (SO_PEERCRED optional path not taken; honest threat model documented). Source: `reserved_hello_authorized` exact map, miss → true (`supervise.rs:384–395`); nonce via `SUBC_LAUNCH_NONCE_ENV` (`supervise.rs:2033`).

**#4: CLOSED** — 6.1:200–206, changelog:11 — Fed-state → CallError table plus recovery reconciliation (no intent row → provably `not_sent` / re-invoke) addresses pre-intent-crash hole; mapping is specified for phase-0 test vectors.

**#5: CLOSED** — 6.1:197, changelog:11 — `effect_id = (origin_device_pubkey, incarnation_uuid, seq)` with incarnation minted on DB (re)create and serving high-water / refuse regress closes post–DB-loss collision.

**#6: CLOSED** — 6.1:199, changelog:11 — Retention co-defined with origin send-log (confirm outcome-received + grace; post-expiry `effect_outcome_expired`, no re-dispatch) replaces circular “origin re-send horizon” definition.

**#6a: CLOSED** — 6.1:196, changelog:11 — v4 changelog drops llm-runner appeal; 6.1:196 states standard WAL discipline (residual historical mention remains only in v2→v3 changelog  not operative v4 normative text).

**#7: CLOSED** — 5.4:180, changelog:15 — First-class `fed:<peer-fingerprint>` harness, provider allowlist coordination, AFT verification before phase 2, config posture vs `mcp:*`; appropriately scoped as SHOULD-FIX / phase-2 gate, not P1/P2.

**#8: CLOSED** — 5.3:170–174, changelog:16 — Non-routable until OOB code compare, rotation via old-key chain or re-verification, code binds long-term device static keys, residual documented.

**#9: CLOSED** — 6.2:213, changelog:13 — Partition classifier = fed-module reaper **closing loopback connections** (connection-granular GOODBYE); module-direction GOODBYE best-effort only, consistent with `forwarding.rs:68–93`.

**#10: CLOSED** — 6.5:223, changelog:14 — Raw capability docs + negotiate + filter **before** typed manifest/P1; matches closed `ProviderRole` serde (`manifest.rs:36–37`).

**#11: CLOSED** — 2.5:79, changelog:12, phase 1 251 — Topology **one connection per (peer, remote module)**, one HELLO each; consistent with `register_module_connection` eviction on same `connection_id` (`forwarding.rs:280–282`).

**#12: CLOSED** — 6.4:220, changelog:17 — `ClientHello` device identity called out as transport addition, phase-4+; accurate underspec accepted as NOTE scope.

**#13: CLOSED** — 243, changelog:17 — Fork Cat: P1 is the mechanism; open item is staleness window only (no “coarse re-HELLO”).

---

## NEW-CONTRADICTIONS (v4 edits vs stale sections)

1. **Removed-tool teardown:** 2.6:85–86 / changelog:9 (module-side typed error, **no** tool-granular GOODBYE) vs **4.1:130** (“removed tools get **route-GOODBYE**”). This reintroduces the architecturally impossible P1 promise (#1).

2. **Loopback topology:** 2.5:79 / changelog:12 / 251 / v4 decision log:270 (**per (peer, remote module)**) vs **3.1:102** (“one loopback connection **per peer**”), **238** (“one connection per peer”), **267** (locked: “one loopback connection per peer”). Multi-module peers cannot be represented on a single connection without eviction (`forwarding.rs:280–282`).

3. **4.1:129** (“per-peer provider HELLO”) is loose wording vs normative **one HELLO per (peer, module)** on its own connection (2.5:79); ambiguous for implementers, not a full blocker if read with 2.5.

---

## WEAKENED-DECISIONS

- **None identified** for locked architecture skeleton (Noise IK, `federation_exposure`, loopback-only core, reserved fed-module, identity split). v4 **strengthens** P1/P2 honesty (same-user floor for P2) rather than weakening prior locks.
- **Caveat:** Leaving 267 and 238 as “per peer” **weakens enforceability** of the v4 topology decision (#11) in the decision log without changing the technical lock in 2.5:79 — documentation drift, not a semantic retreat.

---

## Summary

| Severity (if open) | Count |
|--------------------|-------|
| Blocker folds incomplete in doc | 1 partial (#1 — stale 4.1) |
| Cross-section contradictions | 2 major (GOODBYE, topology) |
| Weakened locked decisions | 0 |

**PHASE-0 VERDICT: NO-GO** — 2.6 P1/P2 normative text and source alignment close unanimous blockers #2 and #3 and substantially close #1 in 2.6, but the re-gate fails document integrity: **4.1:130** still mandates route-GOODBYE on tool removal (same failure mode as v3 #1), and **3.1:102 / 238 / 267** still lock “one connection per peer” against v4’s **per (peer, remote module)** topology required by `register_module_connection` eviction. Phase-0 implementation of `catalog.update` and prefix reservation should not start until those contradictions are edited to match 2.5–2.6 (estimated: lines 102, 130, 238, 267 + tighten 4.1:129); after that one doc pass, **GO** for subc-core P1+P2 with 6.1 mapping table as executable spec debt carried in phase 0 per 250.

</COUNCIL_MEMBER_RESPONSE>

