
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