---
task_id: bg_faba0f01
agent: council-member
session_id: ses_0ce29238dffenUvuJFGM8bLqc3
parent_session_id: ses_0ce29eec3ffej0uyjk5swjgaZj
status: completed
completed_at: 2026-07-05T10:37:44Z
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

[assistant] 2026-07-05T10:33:23Z
**Identifying contradictions in the document**

I need to check the document for contradictions. Right away, I notice a conflict: §4.1 line 130 mentions removed tools getting route-GOODBYE, which contradicts the v4 P1 guidelines in lines 84-86 about no tool-granular GOODBYE. That's definitely a point I need to flag. 

Additionally, §8 line 238's claim of one connection per peer seems to contradict other lines. Overall, I’m seeing multiple inconsistencies, especially surrounding the definitions of peer connections. This needs more exploration.
**Clarifying findings and accuracy**

I’m wondering if stale references might weaken my conclusions, and I need answers to support each finding carefully. For finding #11, could it be considered PARTIAL due to those stale sections? It’s essential that I remain precise in how I categorize these findings and ensure my logic is clearly supported. I think I should double-check what is stale and how it could affect the overall analysis for accuracy. This is a crucial point to address!

[assistant] 2026-07-05T10:33:46Z

[assistant] 2026-07-05T10:33:55Z
**Evaluating changelog details**

I’m noticing that the grep document output is truncated due to long lines, but I see there's a need to find exact information about "llm-runner proven" which appears in line 23 of the v2-v3 changelog. Line 23 mentions some contradictory text about appeal dropped, which seems out of date. 

I need to confirm if #6a is also part of the 13 findings mentioned in v3 and check that it’s clearly listed. It appears these findings are numbered #1 through #13, including #6a. So, I'm planning to evaluate all of these findings.
**Analyzing document status**

It seems that while the fold in §2.6 closes, there's a stale contradiction in §4.1 that leaves the document not fully closed — maybe I should consider it partial. I need to cite sources: forwarding.rs route binding isn't covered by tools in lines 43-60, and there are other issues with module connections, particularly in lines 1123-1195.

Line 130 is conflicting with the source, making it dangerous for progress. The user intends for phase 0 to be gated on regate, so I have to determine if we can move forward or not, especially since there's a P1 blocker remaining.
**Evaluating phase 0 P1 implementation**

I'm considering that the phase 0 P1 implementation could utilize §2.6 lines 84-86 and then build on phase line 250. However, I can't ignore that line 130 presents a contradiction. This inconsistency could pose a risk, so I need to weigh whether I should proceed with the implementation under these conditions or hold off until that issue is resolved. It's important to ensure that everything aligns before moving forward.

[assistant] 2026-07-05T10:34:16Z

[assistant] 2026-07-05T10:34:23Z

[assistant] 2026-07-05T10:34:28Z
**Analyzing claims mapping**

I'm noticing that the v4 line 200 claims mapping is closing, but it uses a "recovery reconciliation" mechanism. It mentions recovery queries for the "sent without outcome," but before that, there’s no sent row, which complicates reconciliation. Hmm, this feels like a subtle issue. I'm wondering about flagging it as PARTIAL. While the v4 claims #4 seems closed, I need to weigh the implications of flagging it as partial, especially since phase0 relates to specific test vectors, not implementation.
**Evaluating retention window definitions**

I see that there's a mention of a "dedup-ledger retention window" that seems circularly defined, which raises questions about its clarity. The status line refers to a retention co-definition that isn't clearly numeric. The issue of bounded grace duration seems unresolved, especially since the audit standard points out needing a numeric staleness window. While the concept of "co-defined" might work in design, I’m curious how this affects the implementation. Overall, it seems like the issue can be considered as "closed" despite some ambiguity remaining.
**Assessing mechanism closure**

I see that the mechanism is present, so I can mark this as CLOSED. However, it's worth noting that while there's no numeric grace period, it doesn't appear to be circular. If I were being stricter, I might label this as PARTIAL due to the absence of a concrete number. Now, for the #6a llm-runner precedent, it seems unverifiable if v4 states that "mechanics are standard WAL discipline," especially since the changelog mentions that the appeal was dropped.
**Considering documentation needs**

I need to find a source for "subc-core opaque body" line docs related to architecture body splices. Maybe I should think about using the aft_outline, but it doesn't seem necessary. I should really focus on point #4 and consider searching for current control regarding "not_sent tombstone," though there's no code available yet because it's for future use. Ultimately, I can cite the source taxonomy only, so I need to decide on a final verdict.
**Identifying contradictions**

I’m noticing a potential contradiction involving the "per-peer provider HELLO" and the "per (peer,module)" terminology. It mentions re-registering under a namespace via this per-peer provider, but shouldn't that be per module instead? The message in line 130 about removing the tools route also seems relevant. I see that line 129 indicates re-registering exposes B's catalog, but this could imply the need for a clearer distinction here, especially regarding module vs. peer connections. I'll flag these contradictions!
**Clarifying potential contradictions**

I'm noticing a contradiction between line 102 stating "one loopback connection per peer" and the idea of "re-exporting each peer's exposed catalog" as singular. Shouldn't that be plural? I also want to think about incorporating line 180, referencing AFT from the docs instead of the source. Regarding the user’s request for a "finding by severity summary," their format doesn't need total severity, but it should include a concise summary. I’ll plan for 13 verdict lines while aiming to be thorough yet clear!
**Organizing findings and verdicts**

I see that for each of the 13 findings, I need to provide a verdict line, which could be helpful to keep it concise. I might want to include the exact text but possibly in an abbreviated form. For example, I could reference "§2.6 L84-L86 'No tool-granular GOODBYE...'" and "§4.1 L130 'removed tools get route-GOODBYE.'" This way, it keeps the information clear while staying focused on what's essential!
**Evaluating findings**

I'm looking at the original note about "underspecified (phase4+)," so it seems that confirming "phase4+" is acceptable. There might be a partial component, possibly controversial, relating to #4. If v4 line 203 suggests that the "not_sent tombstone consumer can query," I'm considering if there might be a mechanism here. However, it seems like there may not be a durable key involved. I need to clarify these points further.
**Considering audit format**

There's a potential issue with the mandatory audit format since it specifies "Finding 1: [Title]." However, the user has requested something different. I want to make sure I respect the user's preferences while also staying compliant with any mandatory requirements. It’s a tricky balance, but I’m committed to finding a solution that satisfies both the user and the standards. I'll focus on tailoring the format to meet the user's needs!
<COUNCIL_MEMBER_RESPONSE>
## Per-finding confirmation

#1: PARTIAL — 2.6 L84-L86 says “No tool-granular GOODBYE,” but 4.1 L130 still says “removed tools get route-GOODBYE” — 2.6 matches source (`forwarding.rs` L43-L60 route keys lack tool identity), but L130 reintroduces the impossible promise. Confidence: high.

#2: CLOSED — 2.6 L84-L86 says P1 replaces only `provides` and rejects `module_id`, role kind, `concurrency`, `control_ops` changes — this matches source: concurrency is captured at HELLO (`control.rs` L619; `forwarding.rs` L169-L174, L298-L305) and `control_ops` are stored/read separately (`registry.rs` L78-L83; `control.rs` L1260). Confidence: high.

#3: CLOSED — 2.6 L88-L92 defines delimiter prefixes, exact-over-prefix precedence, owner module nonce mapping, and says P2 is not a same-user barrier — this correctly addresses current exact-only authorization (`supervise.rs` L384-L395) and env nonce injection (`supervise.rs` L2023-L2033). Confidence: high.

#4: PARTIAL — 6.1 L200-L206 adds the fed-state→CallError table, especially L203 “NO intent row… emits a durable `not_sent` tombstone” — the table exists, but if no intent/effect row was durable, v4 still does not specify the durable correlation key/API by which recovery can emit/query that tombstone; current `CallError` remains only the 4 variants (`consumer.rs` L581-L593). Confidence: medium-high.

#5: CLOSED — 6.1 L197 defines `effect_id = (origin_device_pubkey, incarnation_uuid, seq)` and serving-side seq fencing — this closes DB-loss/restart collision by adding a durable incarnation epoch. Confidence: high.

#6: CLOSED — 6.1 L199 co-defines ledger retention with origin outcome-received confirmation plus grace and makes post-expiry re-arrival a typed ambiguity refusal — this removes the circular “max resend horizon” definition. Confidence: high.

#6a: PARTIAL — 6.1 L196 now says the WAL mechanics stand on their own, but v2→v3 changelog L23 still says it is “borrowing llm-runner’s proven intent-log discipline” — the unverifiable appeal remains in the doc. Confidence: high.

#7: CLOSED — 5.4 L180 defines a first-class `fed:<peer-fingerprint>` harness class, required provider allowlisting, default config posture, and an AFT phase-2 verification gate — this addresses the prior absence of a provider-registration story; source confirms unknown/prefixed harness handling is provider-sensitive (`subc-mcp/src/main.rs` L1149-L1156). Confidence: high.

#8: CLOSED — 5.3 L170-L174 gates first contact as non-routable until OOB code confirmation, defines old-key-signed/verified rotation, and binds the code to long-term static keys — this closes first-contact, rotation, and code-binding ambiguity. Confidence: high.

#9: CLOSED — 6.2 L213 makes the fed-module reaper authoritative and closes affected loopback connections, explicitly not relying on module-direction GOODBYE — this matches source: module GOODBYE is best-effort (`forwarding.rs` L68-L93), while module-connection removal releases client routes (`forwarding.rs` L1112-L1120, L1183-L1195). Confidence: high.

#10: CLOSED — 6.5 L223 says raw capability documents are filtered before constructing typed manifests because unknown `ProviderRole` tags fail decode — this matches `manifest.rs` L34-L39. Confidence: high.

#11: PARTIAL — 2.5 L79 and  L251 correctly choose one connection per `(peer, remote module)`, but 3.1 L102, 4.1 L129,  L238, and  L267 still say per-peer/per-peer HELLO — source requires separate connections because `register_module_connection` evicts a prior module on the same connection (`forwarding.rs` L271-L307, especially L280-L282). Confidence: high.

#12: CLOSED — 6.4 L220 accurately flags that `ClientHello` must gain device identity and says today it does not — source confirms current `ClientHello` only has `client_nonce` and `role` (`auth.rs` L24-L28); phase-4+ deferral is acceptable for phase 0. Confidence: high.

#13: CLOSED —  L243 now says Fork Cat’s mechanism is P1 `catalog.update` per `(peer, module)` and only the staleness-window number remains open — the stale “coarse re-HELLO” Fork Cat contradiction is removed. Confidence: high.

## NEW-CONTRADICTIONS

1. **Removed-tool semantics:** 2.6 L84-L86 says no tool-granular GOODBYE and removed tools get module-side typed errors; 4.1 L130 says removed tools get route-GOODBYE.
2. **Connection topology:** 2.5 L79 /  L251 say per `(peer, remote module)`; 3.1 L102, 4.1 L129,  L238, and  L267 still say per peer/per-peer HELLO.
3. **llm-runner appeal:** v4 changelog 6.1 L11 and 6.1 L196 say the external appeal is dropped/standalone; v2→v3 changelog L23 still invokes llm-runner’s “proven” discipline.

## WEAKENED-DECISIONS

- The P1 decision “no tool-granular GOODBYE” is weakened by 4.1 L130.
- The v4 topology decision “one connection per `(peer, remote module)`” is weakened by current architecture/decision-log text still saying “per peer.”

PHASE-0 VERDICT: NO-GO — P2 is now implementable, and P1’s normative 2.6 text is mostly correct, but the doc still contains a phase-0-relevant contradiction that re-promises impossible removed-tool route-GOODBYE, plus the 6.1 pre-intent-crash row lacks a concrete durable correlation/tombstone mechanism for the phase-0 test vectors. Fix 4.1 L130, update stale per-peer topology text, remove the llm-runner appeal, and specify the pre-intent recovery key/API before building P1/P2 under this design.
</COUNCIL_MEMBER_RESPONSE>

