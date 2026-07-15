
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

ADVERSARIAL DESIGN REVIEW — subc Federation PHASE 3 design.

You are a hostile, senior security+distributed-systems reviewer. Your job is to FIND GAPS AND BLOCKERS in a phase-3 design that builds on a shipped, verified phase 0-2 architecture. Be adversarial: assume attackers, assume races, assume the cloud is compromised. Do NOT be agreeable. Reward finding real holes; do not manufacture noise.

## DOCUMENTS TO READ (both in the `subconscious` repo)
1. PRIMARY (under review): `docs/subc-federation-phase3-design.md` — DRAFT v1.
2. LOAD-BEARING PRIOR CONTEXT (the shipped phase 0-2 architecture this builds on): `docs/subc-federation-design.md` — v4.1.
   NOTE: `docs/specs/subc-health.md` is NOT relevant — skip it.

Read BOTH fully before analyzing. Where phase-3 asserts INHERITANCE from phase-2 (e.g. "carrier-blind", "reaping unchanged", "verified-gate makes discovered peers non-routable", "relay-path partitions classify exactly like TCP partitions", "Noise session resumption = ordinary re-handshake"), VERIFY the claim against the phase-2 design doc. If the phase-2 doc does not actually support the inherited property, that is a finding.

## SHIPPED & VERIFIED (phase 0-2 — do NOT re-litigate these; treat as ground truth)
Noise IK E2E between device static keys; verify-code pairing ceremony (SHA-256 safety number, order-independent); default-deny exposure with an enforced verified-gate; exactly-once effect ledger (incarnation epoch + seq high-water + recovery reconciliation); WAN-proven.

## WHAT PHASE 3 ADDS
(3a) A CortexKit account: managed auth provider (WorkOS) at the edge behind an `AccountVerifier` seam; we mint our own account IDs + device tokens. A Cloudflare Worker + per-account Durable Object (AccountDO) rendezvous: device registry, candidate endpoints, signaling. Candidate-priority dialing lan→public→relay.
(3b) A Noise-over-WebSocket carrier (WsCarrier) and a zero-knowledge ciphertext relay DO (RelayDO) for double-NAT.

## LOCKED DECISIONS — do NOT relitigate (out of scope, will be ignored):
- Managed auth provider at the edge (WorkOS), NOT self-hosted auth.
- account = discovery-only (trust stays key-based via the verify-code ceremony).
- Cloudflare hosting.
- relay-NOT-hole-punching for phase 3.
- one phase gate covering 3a+3b.
Findings that merely re-argue these are noise. Findings about how these decisions are IMPLEMENTED (races, lifecycle, enforcement gaps) are in scope.

## REQUIRED ANALYSIS AXES — hunt in each:

(1) SECURITY
- Device-token lifecycle: theft, rotation, revocation races (revoke vs in-flight signaling; token rotation while a peer holds a stale token).
- Enrollment races: two devices enrolling the same pubkey; pubkey re-enrollment after removal (the `revoked_by_account` → re-pair path); account-id resolution races.
- Signaling abuse: connect_request floods; candidate poisoning by a sibling device OR by the cloud; replay of signaling messages (no nonce/timestamp specified?).
- Relay pipe-token properties: single-use enforcement (where/how?), TTL, who can consume whose grant (can device X redeem device Y's pipe_token?), grant issued to "both sides" — symmetric token or per-side?
- Cross from metadata to routability: anything that lets a compromised cloud OR an account-thief escalate from "sees the device graph / can offer a rogue device" to actual routability or plaintext. Test the claim that verify-code makes this impossible in EVERY window (including the pairing UX window where offer-to-unverified is allowed).

(2) CORRECTNESS
- Candidate racing: both sides dialing simultaneously — how does this interact with the phase-2 pubkey tie-break? Can two carriers win simultaneously (TCP session AND WS session both establishing)? What resolves a double-win?
- WS carrier framing: "one fed record per WS message + retained 4-byte length prefix (redundant but kept)." Any ambiguity — e.g. a WS message carrying !=1 record, or a length prefix disagreeing with the WS message length? Is the receiver's parser fed exactly one record per message, and what happens on mismatch?
- Keepalive/reap over a relay pipe with idle teardown: does RelayDO idle-teardown look like a peer PARTITION to the reaper (false-positive partition → spurious GOODBYE / OutcomeUnknown)? Does the effect ledger's recovery reconciliation interact badly with relay reconnects (a reconnect mid-effect: does recovery re-query correctly across a NEW pipe)? Is "Noise session resumption = ordinary re-handshake, cheap" actually true given phase-2 session/incarnation semantics?
- Registry staleness: candidates changing mid-dial; device online flaps; a stale candidate list causing dial to a dead/wrong endpoint.

(3) OPERATIONAL
- DO-per-account hot spots (a busy account funnels all signaling+registry through one DO).
- WS hibernation wake latency vs signaling timeouts (open question 5): can a cold-wake round-trip blow a ~2s candidate timeout or a connect_request deadline?
- Rendezvous DOWN: degradation to static profiles must be AIRTIGHT. Verify that `[rendezvous]`-absent = exact phase-2 behavior, and that a rendezvous that is present-but-down cleanly falls back rather than hanging. Any path where rendezvous-down bricks an otherwise-static-reachable peer?
- Device clock assumptions: TTLs, last_seen, token expiry — any reliance on device wall-clock that a skewed/rolled-back clock breaks?

(4) THE FIVE OPEN QUESTIONS (§10 of the phase-3 doc) — answer EACH with a concrete recommendation:
1. Device-token custody on disk (0600 file next to device key) — sufficient for v1, or fold into the credentials vault immediately?
2. Registry candidates self-reporting — hardening needed against a malicious sibling device lying about its LAN addr?
3. Relay pipe lifetime: per-connection grants vs a standing pipe per peer-pair — is reconnect-per-idle-teardown churn acceptable at keepalive cadence?
4. Should connect_offer require the target to be verified before signaling is relayed (quieter unpaired devices), or is offer-to-unverified needed for the pairing UX? (Current: allowed, loud.)
5. WS hibernation vs signaling latency — acceptable to pay a cold-wake round-trip on first signal to an idle account?

## OUTPUT FORMAT (strict)
Produce a STRUCTURED FINDINGS LIST. For EACH finding:
- **ID + short title**
- **Severity**: BLOCKER / HIGH / MEDIUM / LOW
- **Exact section it hits** (e.g. §5.3, §7, §4.2)
- **The problem** (adversarial, concrete — the attack or the failure mode, step by step where relevant)
- **Concrete fix** (specific mechanism, not "consider hardening")
- If it's an INHERITANCE claim, state whether phase-2 actually supports it (cite the phase-2 section).

Then a section: **ANSWERS TO THE FIVE OPEN QUESTIONS** — one concrete recommendation each.

Then an **OVERALL VERDICT**: GO / GO-WITH-CHANGES / NO-GO, with the must-fix-before-build blockers listed.

Severity discipline: BLOCKER = ships a security hole or a correctness bug that corrupts effects/loses mutations/enables cross-account routability. HIGH = serious but containable / has a workaround. MEDIUM = real gap, not gating. LOW = polish. Rank honestly; a wall of BLOCKERs with no discrimination is a failed review.