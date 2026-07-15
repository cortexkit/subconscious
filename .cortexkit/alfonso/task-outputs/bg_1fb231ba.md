---
task_id: bg_1fb231ba
agent: council-member
session_id: ses_0c38fdc7cffeiBGLfN5Vs3kxjG
parent_session_id: ses_0c390bb4bffeG3XXZm7M7xt8JI
status: completed
completed_at: 2026-07-07T12:14:47Z
---

[user] 2026-07-07T11:56:50Z
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

[assistant] 2026-07-07T11:56:51Z
I need to locate and read through both documents to understand what I'm working with.
I'll start by reading both design documents fully before analyzing.

[assistant] 2026-07-07T11:56:55Z
I'm reading through both documents now to compare the phase-3 design against the phase-2 version. Let me identify the key inheritance claims and verify them, then structure my analysis around the required axes. From the phase-2 design, I'm noting the foundational elements like Noise IK E2E and the TOFU verification ceremony with cloud-intro.

Now I'm examining what phase-3 claims to inherit from phase-2 and checking for gaps. The phase-3 document asserts that everything above the carrier layer is carrier-blind, but phase-2 doesn't actually define a carrier abstraction—it just specifies Noise IK for WAN transport without that seam. I'm working through the inheritance claims systematically to see where phase-3 either properly inherits or diverges from phase-2's design.

There's a real tension I'm spotting around relay idle-teardown and keepalive behavior. Phase-2's reaper classifies partitions based on missed keepalives, but over a relay with idle-teardown, the dynamics shift. If keepalive traffic keeps flowing at phase-2 cadence, the relay never goes idle and never tears down—defeating the resource savings. But if keepalive is suppressed below the idle threshold, the relay can tear down mid-liveness and the reaper would see a partition. This is a correctness issue that needs clarification about how keepalive cadence interacts with relay idle-teardown. Does the reaper even detect a relay partition if keepalive doesn't traverse it? That would create a false-alive signal. This is a genuine gap in the logic. So the doc's claims about keepalive cadence and relay idle-teardown churn are actually contradictory — they can't both hold unless keepalive is suppressed over the relay, which then creates the partition-detection problem.

For the verified-gate concern: phase-3 says discovered peers start unverified and non-routable, and phase-2 backs this with a default-deny federation_exposure gate plus TOFU code comparison. The gate is consistent, but there's a timing risk during the pairing UX window where an offer might reach an unverified peer before the exposure allow-list is applied.

On Noise session resumption: phase-3 claims re-handshake is cheap, but I need to check whether phase-2's IK mode handles incarnation semantics correctly during resumption.

The real issue is recovery reconciliation. Phase-2 only runs it on process restart, not on mid-session relay reconnects. So if a relay pipe tears down mid-effect — intent is durable but the send is unconfirmed — the origin won't re-query the serving ledger to check if the effect actually executed. Instead it waits for the reaper to classify the partition and settle as OutcomeUnknown. But the effect might have already completed on the serving side, and recovery reconciliation would resolve it correctly if it ran. A mid-session relay teardown could spuriously report OutcomeUnknown for an effect that actually succeeded, which is worse for a mutator since the consumer can't safely retry.

Phase-2 has the same behavior for TCP partitions, but Phase-3 adds frequent idle-teardowns and reconnects, making this window much more common. The doc claims reconnect is "cheap" but glosses over the fact that reconnect mid-effect needs the same recovery reconciliation as a restart — and it doesn't specify that the fed-module actually runs reconciliation on reconnect. This is a real gap: Phase-3 should mandate that a relay pipe reconnect triggers per-effect reconciliation (querying the serving ledger for in-flight effect IDs over the new pipe) rather than relying solely on reaper-driven OutcomeUnknown settlement. I'm flagging this as HIGH priority.

On the serving side, the high-water and incarnation behavior: a re-handshake creates a new Noise session but incarnation stays the same since the database persists. The serving side maintains per-(pubkey, incarnation) high-water, so a re-sent in-flight effect over the new session hits the dedup ledger correctly — as long as the origin actually re-sends instead of settling as unknown. The interaction between reaper settlement and reconciliation is where the tension lies.

Now I'm moving into the security axis. Device-token lifecycle is the first area: revocation is supposed to invalidate the token, but the spec doesn't say whether revocation actively closes a live WebSocket that the revoked device holds. A revoked device could keep signaling until the connection drops, potentially offering rogue pairings during that window. The fix is explicit: after revoke, the DO must terminate the device's WebSocket and drop it from the online set. This is MEDIUM severity since the rogue device can't speak Noise and therefore can't route, but it's still a containment issue.

Token rotation is another gap — the mechanism is unspecified, and there's no clarity on whether a grace window exists where the old token still works during rotation. That's MEDIUM-LOW priority since the token only handles rendezvous auth, not trust itself.

For token theft, the threat model acknowledges it's possible but revocable. However, a stolen token gives the attacker write access to the victim's registry entry: they can poison the candidate list with attacker-controlled endpoints, set online=false for a DoS, or flood connect_requests to siblings. Candidate poisoning is the key risk — since candidates are self-reported, the token-thief can redirect the victim's dials to an attacker endpoint. The Noise handshake will fail because the attacker doesn't have the private key, so there's no MITM, but it's a DoS or redirect. Public candidates are server-stamped, but LAN and relay candidates are self-reported, so those are vulnerable. Combined with the revocation-closes-WS gap, the attack window is unbounded until the operator notices. That's MEDIUM severity.

Now looking at enrollment races: the spec says enrollment binds account_id and device_pubkey in §4.2, but it doesn't state whether pubkey is unique per account. If two devices enroll the same pubkey, you get ambiguous registry entries. An attacker could enroll a pubkey they don't own the private key for — they only need a valid provider JWT for the account plus any pubkey — creating a registry entry for an arbitrary key. Without the private key they can't complete the Noise handshake, but the registry is still poisoned.

The more concrete threat is enrollment collision: if the registry is keyed by device_pubkey and an attacker with account access re-enrolls an existing device's pubkey, it either overwrites or collides with the original entry. If it overwrites, the attacker hijacks the registry entry and redirects candidates. Noise still prevents MITM, but DoS and redirect are possible. The spec mentions that removed devices flip to revoked_by_account and require re-pair if re-enrolled, but it's unclear whether that's enforced. A compromised cloud could suppress the revoke delta, keeping peers thinking the device is valid, or forge a revoke entirely. Suppressing the revoke means the removed device stays trusted at peers, but it still needs its private key to route — removing a device from the account doesn't remove its pinned key at peers, so "device remove" for security purposes is incomplete.

The core issue is that revocation depends on the cloud delivering the registry delta, which contradicts phase-2's principle that the cloud is discovery-only and trust is key-based. Phase-2 revocation used signed tombstones, but phase-3 registry deltas aren't clearly signed. A compromised cloud could forge a revoke delta to DoS a legitimate device by flipping peers to non-routable, or suppress a real revoke to keep a stolen device routable. The revoke mechanism's reliance on unsigned deltas is the core issue: un-pinning is a trust decision that phase-2 promised to sign (§6.2/§6.4), but phase-3 treats all signaling as non-trust-bearing and leaves deltas unsigned—a direct contradiction that enables a compromised cloud to forge revokes or suppress them to keep stolen devices routable. This is HIGH severity.

I'm also spotting a race condition in account-id resolution: concurrent first-logins with the same provider subject could mint two separate account_ids, splitting devices across accounts and breaking discovery. The minting logic needs to be atomic and idempotent on the subject, likely via a unique constraint in a single-writer store (global DO or D1), but the spec doesn't clarify where account_id creation happens or how it's keyed. That's MEDIUM. And there's a potential for connect_request floods in the signaling layer that I'm starting to examine. Forcing relay also amplifies DoS since it becomes a single chokepoint the cloud controls, though the metadata exposure is already accepted—that's a medium-low risk. Signaling message replay is low-risk given TLS and per-device authentication, though the cloud could theoretically replay stale candidate sets, which ties back to registry staleness. For relay pipe-tokens, the RelayDO enforces single-use with short TTL, and each side gets its own token for added security.

The real vulnerability emerges if pipe_tokens aren't bound to device identity—a token thief or malicious sibling could open many pipes to drain relay capacity and amplify costs. Phase-2 required quotas, but phase-3's RelayDO spec doesn't restate per-device limits on pipe creation, leaving an inheritance gap. That's medium-high risk; the fix is binding pipe_tokens to device identity.

There's also an ambiguity in the grant structure: §5.3 shows a single pipe_token field in the relay_grant message, but §7 claims per-side tokens. If both sides receive the same token, either could redeem both slots or a token-thief could occupy both ends—that's a medium-severity confusion that needs explicit distinct tokens per side.

Now I'm tracing the core claim about verify-codes blocking account-thieves from reaching routability. The pairing UX allows offers to unverified devices, which means a rogue device enrolled by an account-thief can send connect_offers to the victim's real devices. The victim sees a pairing prompt with a verify-code, and the safety number derives from both devices' static keys.

Here's the critical issue: the verify-code ceremony assumes humans compare two independent displays, so an active MITM would show different keys on each side and the codes wouldn't match. But in the account-thief scenario, there's no MITM—the operator is deliberately pairing with what they think is a new device. The rogue device can compute the exact safety number from its own pubkey plus the victim's pubkey (both known from the registry), then display that matching code on its screen. The operator comparing their real device's code against the rogue's screen sees a match, even though the rogue is malicious.

This means the verify-code ceremony doesn't actually protect against an operator being tricked into pairing a rogue device they believe is theirs—it only protects against MITM key-substitution. The phase-3 claim that "the rogue device's verify-code will not match anything the operator's real devices display" is incorrect for a first-time pairing; there's nothing to compare against yet. The real protection is purely human judgment—recognizing whether you initiated this pairing—not cryptography.

But this might not be a new risk compared to phase-2. Phase-2 already documented that the code comparison is the gate, and if a user confirms it, they're trusting the cloud. The distinction phase-3 adds is that an account-thief (not just a compromised cloud) can now enroll a rogue device themselves.

The key finding is that §4.3 and §8 overclaim what the verify-code protects against. The loud join events and operator vigilance are the real mitigations, not the safety number itself—the attacker can compute and display it correctly. A concrete fix would require that pairing a newly-enrolled device also needs confirmation from an already-verified device, forcing the operator to approve from a device they already control. This is a high-severity issue since it's the central security claim being overclaimed, though the loud-join plus human vigilance does provide real mitigation, and trust remains key-based, so it's not full routability without human action.

The account-thief already has the provider login and can enroll a rogue device, but for it to become routable the operator must actively confirm a verify-code. If the UX makes this a deliberate pairing flow, a vigilant operator can decline. The overclaim about crypto preventing the attack could lead builders to under-invest in UX and existing-device-attestation safeguards. There's also a secondary risk: the attacker could enroll a rogue device with the same name as a real device and poison the registry if the UX foregrounds the name over the pubkey fingerprint—the registry shows both, but if the app displays the name prominently, the operator might confirm the rogue thinking it's their real device. The pubkey is authoritative, so the UX must foreground the fingerprint.

On the correctness axis, there's an ambiguity in the spec about candidate racing. The pubkey tie-break determines who initiates, but the spec also says both sides race candidates. If both sides simultaneously establish carriers—TCP from A→B and WS via relay from B→A—you could get two carriers completing concurrently, creating two Noise sessions. The effect ledger keys on effect_id not session, so effects don't duplicate, but you'd have two live sessions and two loopback connection sets. When both carriers try to register the same namespaced module_id, the registry rejects the duplicate, leaving the second carrier in a half-open state. This is a real correctness bug: the phase-3 spec relies on the pubkey tie-break to pick a single initiator but doesn't actually enforce it.

I need to verify whether phase-2 even specifies a pubkey tie-break for simultaneous dials. Looking back at the phase-2 doc, I see role assignment by reachability in the NAT section, but I can't find an explicit pubkey tie-break mechanism. Phase-3 claims "pubkey tie-break as phase 2," but that inheritance claim may not hold—phase-2 assigns roles asymmetrically based on reachability, not pubkey comparison. This is a gap worth flagging.

For the WS carrier framing, there's ambiguity around the 4-byte length prefix and message boundaries. The spec says one record per WS message with the prefix kept for carrier-agnostic parsing, but if the stream parser (identical to TCP's) receives message boundaries instead of a continuous stream, there's risk of desynchronization if a message has trailing bytes or a prefix/length mismatch. Worse, a malicious sender could coalesce multiple records into one WS message, bypassing the per-message frame cap accounting.

The fix is strict validation: reject any WS message where the declared length prefix doesn't match the actual message length minus 4, and ensure exactly one complete record fills the entire message; drop the connection on violation. Golden vectors need to include mismatch-rejection cases.

For keepalive and reaping over relay, there's a timing tension: if the relay's idle teardown timer is shorter than the keepalive interval, the relay tears down between keepalives. On the next keepalive attempt, the reaper sees a dead pipe and triggers reconnect. If reconnect is slow (like WS cold-wake hibernation, potentially seconds), it could exceed the 3× reap window, causing a false partition detection and spurious GOODBYE messages with OutcomeUnknown for in-flight requests.

The phase-3 spec claims relay-path partitions behave like TCP partitions with unchanged reaping, but relay idle-teardown is a mechanism TCP doesn't have, so the classifier can false-positive. This contradicts the inheritance claim. The fix is either suppress partition declaration during relay reconnect (treat it as a distinct "reconnecting" state with a longer budget) or keep the pipe alive with keepalive that resets the relay idle timer—pick one and document it clearly.

For registry staleness, stale candidates just timeout and fall to the next one, which is bounded. But a stale public candidate where the device moved networks means the observed IP now belongs to someone else (NAT reuse or DHCP churn), so dialing reaches the wrong host. Noise IK fails due to key mismatch and falls back to relay, so no actual misconnection occurs, but it wastes 2 seconds per stale candidate. Online flaps cause registry churn and re-dial storms.

There's also a privacy concern: the server-observed public IP gets stored in the registry and shared with sibling devices, so a compromised sibling or account thief learns the device's public IP and rough location. That's metadata leakage, accepted as low risk.

On the operational side, one DO per account means a busy account with many devices funnels all signaling through a single-threaded Cloudflare DO, creating a hotspot. For personal accounts it's fine, but at team scale with many devices, the registry, signaling, and all WebSocket traffic through one DO could bottleneck under connect_request floods. That's medium risk given the no-rate-limit finding.

Cold-wake latency for hibernated DOs plus re-establishing the target device's WebSocket delivery could add latency that conflicts with the ~2-second candidate timeout for the data path.  If the DO is hibernated when a connect_request arrives, the first signal pays the cold-wake cost; if that exceeds the connect deadline, the first attempt fails and triggers a retry. The target device's WebSocket also needs to be re-delivered through the hibernated DO, which could take hundreds of milliseconds to seconds. Medium risk, fixable with generous first-signal deadlines and client retry logic, or by keeping the DO warm while any device WebSocket is connected.

For rendezvous degradation, the doc addresses what happens when rendezvous is absent, but not when it's configured but unreachable. If the control client blocks startup waiting for the registry snapshot and rendezvous is down, federation could stall entirely even for peers reachable via static addresses. There's no specified timeout for rendezvous connection attempts.

The fix is to make rendezvous connection async and non-blocking so static candidates can dial immediately regardless of rendezvous state, with an explicit timeout and fallback to static-only mode. High availability risk.

Device clock assumptions pose another threat: TTLs, last_seen timestamps, and token expiry all depend on wall-clock time. Most TTLs are server-enforced (good), but any device-side TTL—like the device deciding when to re-probe candidates or caching a registry snapshot with a client-side staleness timer—could break if the device's clock jumps due to sleep/wake or NTP step adjustments, causing the reaper to fire prematurely or miss keepalive intervals.

The concrete risk is low: if a device's wall-clock rolls backward, a stale candidate could be treated as fresh and the device might dial a dead endpoint, but Noise authentication and the 2-second timeout contain this. The mitigation is to ensure all federation timers use monotonic clocks instead of wall-clock, while keeping TTL enforcement server-side.

I'm also checking enrollment authorization: any valid account JWT can enroll any pubkey without proof-of-possession of the private key. This means an account-holder can enroll a pubkey they don't control, which doesn't directly enable cross-account attacks but does create registry pollution and collision risks. Since routability requires Noise (private key), an unproven pubkey can't actually route, but the real issue is whether enrollment should require a signature by the device_pubkey over a challenge to bind the token to a real keyholder.

For the revoked-device re-pair path, the concern is whether un-pinning on revocation is enforced locally or just flagged in the registry. If the peer only marks the key non-routable but keeps the pin, a compromised cloud could flip it back to routable without requiring a fresh verify-code ceremony. The safe design requires the local profile to drop the key on revoke and a fresh human-verified ceremony on re-pair.

On device-join events, while the cloud pushes notifications to all enrolled devices, a compromised cloud could suppress the join event for a rogue enrollment, leaving only the verify-code as defense. But if the attacker can compute a matching code and the compromised cloud sends a connect_offer from the rogue device, the operator's real device shows a pairing prompt—and if the operator confirms the (attacker-computed) code, the rogue becomes routable. The join-event suppression combined with code computability means a compromised cloud with account access could get a rogue device paired if it can trigger the operator to confirm during a pairing flow.

This strengthens the verify-code overclaim finding and reveals that loud-join events, being cloud-delivered, aren't a reliable defense against a compromised-cloud actor. The existing-device attestation fix helps here by requiring an already-verified device to approve new pairings, which the cloud can't forge.

I'm also separating the reconciliation-on-reconnect concern as its own HIGH finding since it touches effect settlement and correctness. For the relay and reap behavior, I'll combine those into one finding with two facets. On Noise session resumption, Phase-2 IK doesn't support resumption—it's just a fresh handshake (2 messages, 1.5 RTT). Over a relay with cold-wake, the re-handshake also pays the relay reconnect and device-orchestrator wake cost. The key question is whether a re-handshake resets the effect ledger's session state.

The ledger keys on effect_id (incarnation), which survives across sessions, and the per-(pubkey, incarnation) high-water mark persists in the ledger database rather than session state, so re-handshaking doesn't reset fencing. The claim holds, but it's incomplete—a re-handshake mid-effect requires reconciliation to determine whether the origin re-drives in-flight effects or drops them to OutcomeUnknown. Each new Noise session has fresh ephemeral keys and nonces, so there's no replay risk across sessions.

Now I'm looking at a potential candidate disclosure issue: when a rogue device sends connect_request, the target receives connect_offer with the initiator's candidates, then responds with connect_accept containing the target's own candidates. This means the target's LAN and public IPs leak to any unverified account-thief who can send a connect_request.

But wait—the registry snapshot already exposes all candidates to same-account sibling devices anyway (that's the accepted cost of the account = discovery model). So an account-thief sees the full candidate list from the snapshot without needing to send connect_request at all. The real question is whether exposing LAN IPs and public endpoints to a potentially compromised account is an acceptable risk given the threat model around stolen provider accounts and rogue device enrollment.

Now I'm stepping back to finalize the findings and prioritize by actual severity. A BLOCKER would be something that ships a security hole or correctness bug—corrupts effects, loses mutations, or enables cross-account routability. The verify-code overclaim doesn't quite hit that bar because it requires operator confirmation and only affects the same stolen account, not cross-account access. That's HIGH, not BLOCKER. I'm also looking at the simultaneous carrier double-win scenario as another candidate. On reconnect mid-session, if the origin neither re-queries nor re-sends, the effect stays stuck as `sent` until a full restart, leaving the consumer in OutcomeUnknown—safe-side but avoidable. For the WebSocket framing ambiguity, the relay can't forge valid Noise frames since it only forwards ciphertext, and Noise's per-frame integrity plus anti-replay protection means any tampering fails AEAD validation and drops the session safely. The real risk is a spec ambiguity that leads implementations to skip strict framing validation, causing subtle desync if a sender coalesces multiple records per message—this is a spec-hardening issue rather than a fundamental corruption vector.

Unsigned revoke and registry deltas create a genuine security gap: a compromised cloud can suppress revocation, leaving stolen devices routable at peers who don't receive the delta. While manual un-pinning is still possible and the trust model remains key-based, automatic revocation becomes cloud-dependent, violating the phase-2 promise of signed tombstones. The rendezvous availability issue—when the rendezvous is down, static-reachable peers become unreachable—doesn't corrupt effects but significantly impacts availability.

Moving through the remaining findings: enrollment without proof-of-possession risks registry pollution, signaling lacks rate limits (enabling connect_offer floods and notification DoS), relay quotas aren't enforced per device or account, pipe_token semantics are ambiguous across sections, live WebSocket connections don't close on revocation, account resolution races can create duplicates, and candidate/network-map data leaks full LAN and public IP maps to account thieves. Device clock handling and per-account hotspot operations present lower-severity concerns, as does WebSocket hibernation timing against deadlines.

I'm narrowing down to the highest-signal findings and reconsidering whether any truly qualify as blockers. The strongest candidate remains the unsigned revoke-delta combined with verify-code overclaim: a compromised cloud can render revocation ineffective or inject rogue devices into the routing table.

But examining the double-win scenario more carefully—two simultaneous Noise sessions to the same peer—the effect ledger shouldn't corrupt because only one carrier wins and carries traffic; the duplicate session gets rejected on module_id collision, so the losing carrier's HELLO fails and gets discarded. That's messy but not a true blocker.

The honest assessment is go-with-changes, with a must-fix list: signed revoke and registry deltas with local un-pin semantics, verify-code overclaim requiring existing-device attestation for new-device pairing, non-blocking static fallback when rendezvous is down, tie-breaking simultaneous carriers into single-session establishment, relay idle-teardown versus reap-window false positives, and reconnect-mid-effect reconciliation. These are gating issues that violate inherited phase-2 constraints or create genuine must-fix-before-build gaps, even though none silently corrupts effects across accounts.

Reconsidering the verify-code claim though—the doc asserts in §4.3 that an attacker can't make existing devices trust a rogue one and the verify-code won't match, but I've shown the attacker can compute the matching safety number for a fresh pair. That makes the doc's central security claim factually false and load-bearing on the design, which pushes it toward blocker territory on the design claim itself, though the actual routability still requires human confirmation so it stays same-account rather than cross-account. I'll flag it as high but note the doc's claim needs correction before build so implementers don't under-invest in the existing-device-attestation gate.

The real mitigation is requiring new enrollments to be attested by an already-verified device the attacker doesn't control—like Signal's model where adding a device needs approval from an existing one. An account thief has the provider login but not the private keys of existing trusted devices, so this requirement defeats them and enforces the "loud join" intent.

I'm also noticing phase-2 was honest about the residual risk (skipping code comparison means trusting the cloud), but phase-3 overclaims that the code "will not match" for a rogue device—that's a regression in threat honesty. Now I need to write this up and address the five open questions concretely, starting with device-token custody.

For Q1, a 0600 file is acceptable in v1 since the token only grants signaling and registry access, not Noise encryption—token theft is contained to DoS and metadata leakage. The device key is far more sensitive and already stored as 0600, so the token can follow the same pattern. I should recommend server-side hardening though: bind the token to the device pubkey via proof-of-possession, rotate it frequently, close WebSocket connections on revocation, and apply rate limits to minimize blast radius if stolen. The vault can be a follow-up, not a blocker.

For Q2, the doc correctly identifies that Noise pins identity so a lying sibling can't MITM, but it misses two issues: a sibling can redirect candidates to a third party's address for reflected-dial attacks, and more importantly, candidate exposure lets unverified siblings see all LAN and public IPs. I should recommend stamping what the server can, rate-limiting candidate churn, hiding raw candidates from unverified siblings until after verification or an approved connection, and validating that self-reported LAN addresses are actually in RFC1918 space to prevent using a sibling to point dials at arbitrary hosts.

For Q3, if keepalive traffic traverses the pipe at phase-2 cadence, the pipe never becomes idle, so per-connection grants with idle-teardown won't churn during live sessions—idle-teardown only reaps genuinely dead connections, which is good. The cold-wake latency is acceptable if the signaling deadline is generous (at least 5 seconds, separate from the 2-second candidate timeout) and the client retries once on timeout. The DO stays warm implicitly as long as any device has an active control connection, so the cold-wake only affects fully-idle accounts where the target is offline anyway. The key is not artificially keeping DOs warm—that defeats the cost model—but rather bounding the penalty with a forgiving deadline and retry logic.

For the overall verdict, the changes should proceed, but there are critical blockers: sign registry deltas and define local un-pin semantics without misclassifying revokes; require existing-device attestation for new enrollments to prevent account theft; make rendezvous-down non-blocking with static candidates dialing independently and a bounded connect timeout with fallback; and handle simultaneous connection attempts correctly.

I'm also checking the phase-2 spec to verify specific claims about signed tombstones and pubkey tie-breaks for simultaneous dial scenarios. The doc confirms that phase-2 requires signed revocation tombstones in sections 6.4 and 6.2, but phase-3's registry delta broadcast doesn't mention signing—that's an inheritance violation. Still searching for the pubkey tie-break specification in the simultaneous-dial case. Running targeted searches...
Let me verify a couple of load-bearing inheritance claims against the actual phase-2 text and the wider repo before finalizing.

[assistant] 2026-07-07T12:03:31Z
I'm checking the phase-2 documentation more carefully, particularly the partition classifier section, to see if there's any mention of a pubkey tie-break mechanism for handling simultaneous connections. Phase-3 references this as inherited from phase-2, but I need to verify whether phase-2 actually specifies how to resolve this situation.

Looking at the phase-2 doc, I see the NAT specifics mention that the reachable peer acts as server while the NAT'd peer holds the outbound tunnel, but there's no explicit tie-break rule for simultaneous dials. This appears to be a gap — phase-3's claim about inheriting a pubkey tie-break from phase-2 isn't actually supported by what's documented in phase-2.

I'm also examining how relay-path partitions compare to TCP partitions, particularly around idle-teardown behavior. The keepalive cadence runs over the Noise session, so if keepalives are flowing, the relay pipe stays active. But there's a subtle timing issue: if the relay's idle timeout is shorter than the keepalive interval, the relay could tear down even while keepalives are being sent, which would look like a partition from the protocol's perspective.

The core tension is that over TCP, idle connections don't get torn down by intermediaries, but over relay, the RelayDO idle-teardown will actively close the pipe. If keepalives run frequently enough to prevent idle-teardown, that defeats the cost-saving purpose of idle teardown. But if they don't, the relay can tear down the transport while the session is still logically alive, and the fed-module's reaper would treat that as a partition, closing loopback connections and sending GOODBYE to consumers.

The real issue: phase-2 defines keepalive as application-level over the Noise session, and the reaper is the authoritative partition classifier. When a relay tears down the pipe for idle, reconnection requires a fresh grant and re-handshake. If a keepalive is due during that teardown/reconnect window, it gets missed and the reaper fires, marking the peer partitioned — but the doc doesn't specify a transport-reconnect state distinct from actual partition, so there's no way to distinguish between the two.

Additionally, phase-3 introduces a tie-break mechanism for simultaneous dial attempts using pubkey ordering, but the thinking is cut off examining how connect_request initiation works. Signaling operations like connect_request and connect_offer aren't authenticated beyond the device token, so there's a risk that a sibling device could forge the `from` field unless the server stamps it — I need to verify the DO actually validates this rather than relaying client-supplied values. That's a medium-severity gap. For relay pipe_tokens, each side gets its own single-use token with a short TTL, which prevents cross-device redemption, but I'm checking whether the single-use enforcement is robust against race conditions or a compromised cloud that could mint its own grants — though even then the attacker only gets ciphertext since the Noise session is end-to-end encrypted.

Device token theft on a multi-user system is a real risk since another user with file access can impersonate the device for signaling and candidate poisoning, even though they can't speak Noise without the private key. The fix would be to require proof-of-possession of the device key when using the token, rather than letting a stolen token grant full rendezvous identity. For revocation, there's an unspecified race between revoking a device and in-flight relay grants — a revoked device could keep an existing relay pipe alive if the pipe doesn't re-check the token on each use, though the verify-code pinning does flip the device to non-routable status.

The real issue is whether revocation actively tears down live Noise sessions or just blocks new routes. If it only prevents new dials, a compromised device retains access until the session naturally ends. Since trust is key-based (verify-code pinning) rather than account-based, the account-driven device removal signal must actively close sessions, but the spec doesn't mandate this. Additionally, the revocation delta is distributed through the cloud, so a compromised or uncooperative cloud can suppress the delta and prevent revocation from taking effect — the peer would keep trusting the stolen device because the revocation signal never arrives.

Now I'm looking at enrollment races and account-id collisions. Two devices could enroll with the same pubkey (indicating key theft), but the spec doesn't clarify whether the second enrollment overwrites or collides. There's also a potential race during account creation: if two concurrent first-logins happen for the same provider subject, they could mint separate account_ids instead of converging on one. The re-enrollment path after revocation requires re-pairing, which gates routability, but a stolen provider account could still re-enroll a revoked key — though the re-pair ceremony would catch it. The account-id resolution needs atomicity to prevent duplicate minting.

For the pairing UX window, the real risk is social engineering during the offer-to-unverified phase. A compromised cloud could present a rogue device, and if the operator is tricked into comparing codes with it, they'd pair it — but the verify-code wouldn't match since it binds both static keys. However, the rogue device can still initiate a Noise IK handshake because it knows the real device's pubkey from the registry. The real device won't route tool calls (unverified-gated), but the Noise session establishes. The question is whether catalog federation happens before verification, which would expose the victim's capabilities to the rogue device.

The bigger threat is connect_request floods to unverified targets causing DoS and prompt fatigue, leading users to accidentally pair the rogue. The fix is to gate offer-to-unverified behind a user-initiated, time-bounded pairing window rather than leaving it always-on.

For candidate poisoning, a compromised cloud can stamp a wrong public IP, but Noise IK fails against the wrong key, so it's just DoS, not compromise — the connection falls back to relay. The concern shifts to a sibling device lying about its candidates.

A malicious sibling or compromised cloud could poison the candidate list to point dials at arbitrary internal addresses, creating an SSRF/port-scan primitive where the fed-module connects outbound to attacker-chosen LAN hosts. Even though Noise fails, the TCP connect and handshake bytes hit the internal service. The mitigation is to validate candidates (no loopback/link-local/multicast/broadcast, optionally restrict to the observed subnet) and rate-limit dials.

For the WebSocket framing issue, the ambiguity is that a WS message could carry a length prefix that disagrees with the actual payload length. If the parser trusts the prefix over the WS message boundary, it could desync or over-read the buffer. The spec says "one fed record per WS message" but keeping the length prefix means the parser can read and slice — the question is what happens if a sender violates this invariant.

A malicious relay can split or merge WS messages, and if it merges two Noise-encrypted records into one WS message, the receiver expecting one-record-per-message would mis-slice. However, Noise provides per-message integrity, so merged or split ciphertexts fail decryption and MAC verification, tearing the session. The real risk is a correctness/DoS issue: the parser must deterministically reject length-prefix/WS-length mismatches and enforce exactly one record per message, tearing the session rather than attempting to reassemble across messages. The redundant length prefix becomes a footgun if the parser is shared with the TCP stream parser, which buffers across reads — a WS carrier could accidentally feed a split record across two WS messages, violating the "one record per message" invariant. The recommendation is to specify strict validation and add an assertion to prevent shared codec buffering.

The cold-wake latency of a hibernated AccountDO can blow the ~2s per-candidate timeout if signaling happens before the DO wakes. The initiator's candidate timeout is for the data-plane dial, but the signaling round-trip (request→offer→accept) precedes it; if signaling is slow due to cold wake, the entire dial fails on the first attempt. The fix is to separate the signaling deadline (e.g., 10s) from the 2s data timeout, and warm the DO on the initial `hello` message since both devices hold persistent control WS connections — the DO only hibernates when both are idle, so the first reconnect wakes it. This cold-wake hit is unavoidable on the first signal after both were offline, but decoupling the timeouts prevents the data timeout from prematurely failing the signaling phase.

For rendezvous-down degradation, the concern is whether a peer with a static address in their profile can still dial that static candidate if rendezvous is down, or if the control-plane client blocks startup waiting for registry_snapshot. If all dialing is gated behind a successful rendezvous connect, rendezvous-down would brick even static-reachable peers. The static-candidate path must be independent of rendezvous availability — the implementation should dial static candidates without waiting for the control WS, with a bounded connect timeout and non-blocking fallback. This is a high-severity issue if mis-implemented, so an explicit requirement and test are needed.

For device clock assumptions, most TTLs are server-side (pipe_token TTL enforced by RelayDO, device token expiry checked by Worker, last_seen stamped by server), so a rolled-back device clock doesn't break validation. The main concern is whether any TTL is device-evaluated before presenting to the server — if so, an expired token could be presented, but the server re-checks it anyway, so it's ultimately safe.

On replay protection, there's currently no per-message timestamp or nonce in signaling, leaving the replay window unbounded. This ties to the broader replay finding and is a low-to-medium severity issue.

Effect ledger recovery across a new relay pipe works correctly because effect_id is transport-independent, so recovery queries over a fresh Noise session to the same peer succeed. The interaction with relay idle-teardown is the real concern — if the relay reconnect triggers the reaper and closes loopback connections, a full IK handshake is needed on reconnect, which is more expensive than "cheap" suggests.

The core issue: phase-2's partition classifier treats relay idle-teardown as a partition unless phase-3 adds a distinct transport-reconnect grace state. This causes spurious OutcomeUnknown on in-flight mutations during every idle reconnect, surfacing ambiguous errors to users and churning catalogs. It's not a correctness blocker for effects (exactly-once semantics hold), but it's a real reliability defect that warrants HIGH severity.

The deeper tension is between keepalive cadence and relay idle timeout — if keepalive stays active, the pipe never idles and §7's idle-teardown savings vanish; if keepalive is suppressed to allow idling, the reaper must not fire during that idle window. This needs explicit co-design: either keepalive keeps the pipe warm (making idle-teardown illusory), or an intentional "parked" peer state where the fed-module suppresses the reaper and lazily re-dials on next call with zero in-flight calls by definition. This is a strong finding requiring explicit design.

I'm also flagging a signaling security gap: phase-3 doesn't mention per-device rate limits on connect_request operations, unlike phase-2's relay quotas. A malicious sibling with a stolen token could flood connect_request to all siblings, triggering Noise handshakes and pairing prompts on each victim. This needs signaling rate limits per device — medium severity since phase-2 had relay quotas but phase-3 signaling ops omit them.

On the relay_grant and cloud compromise angle: a compromised cloud could grant a pipe between a victim and its own rogue endpoint, but since it's Noise E2E with ciphertext-only visibility, the cloud already has this capability as the relay itself, so there's no escalation. The CGNAT scenario where a public IP:port routes to a different device is just a DoS via failed Noise handshakes — low risk.

The real correctness gap is in double-connect resolution: if both sides issue connect_request, candidate racing creates two TCP connections and two Noise sessions. When both try to register the same fed module catalog, the second HELLO collides with the first (duplicate module_id rejected by registry), leaving that session half-dead. There's no deterministic tie-break for which session serves calls — this needs explicit resolution logic.

For the five concrete recommendations: the 0600 file token needs proof-of-possession binding to the device key so a stolen file alone is useless, plus eventual migration to the credentials vault. Sibling lying about LAN addresses is a real SSRF/scan vector — harden candidate acceptance by rejecting non-routable addresses and rate-limiting dials. The relay pipe lifetime needs per-peer standing pipes...

Actually, I'm reconsidering the relay approach. Instead of always-warm pipes (cost) or naive teardown (reaper false-positives), I should adopt an explicit PARKED state — tear down the relay pipe on idle, suppress the reaper while the peer is marked reachable-lazy, and re-dial on the next call or keepalive. This handles both the keepalive cadence and the cost concerns. For offer-to-unverified, gate it behind a user-opened, time-bounded pairing window on both ends to kill unsolicited floods and handshake DoS. For WS hibernation, it's acceptable to pay one cold-wake round-trip on first signal if signaling has its own generous deadline decoupled from the per-candidate timeout, and the control WebSocket reconnect warms the DO before any connect request.

The overall verdict is go-with-changes, but the double-connect / simultaneous-dial resolution is a must-fix — it's a high-priority issue because the unsupported inheritance claim corrupts catalog registration and leaves undefined which session actually serves.

Now I'm reviewing the remaining findings: relay idle-teardown versus reaper false-partition (defining the PARKED state), revocation liveness (active teardown on device-remove), device-token proof-of-possession binding, signaling replay/auth and rate limits, WebSocket framing strictness, candidate sanitization, and account-creation idempotency. I'm checking whether any of these are blockers — meaning they ship a security hole or correctness bug that corrupts effects, loses mutations, or enables cross-account routability. The verify-code gate holds across all windows, so there's no cross-account routability blocker, and the effect ledger survives relay reconnect, so the double-connect doesn't corrupt effects.

The honest verdict is no true blocker by the definition, so I'm going with the changes and flagging the HIGH-severity items as must-fixes. But I'm reconsidering revocation: if device-remove doesn't actively tear down a live session and the cloud suppresses the delta, a stolen device with a pinned+verified key keeps full routability until manual unpairing — that's not cross-account routability (the device was legitimately paired), but it's a HIGH gap because the design sells "device remove" as security revocation without active teardown. I'm also re-examining whether double-connect truly gates: if both sessions form and only the second HELLO is rejected, the losing session becomes a zombie holding a socket and relay pipe, which is messy but doesn't corrupt effects or lose mutations. The catalog federation happens over the Noise session, but since unverified peers have an empty allow-list, they can't access any exposed catalog—so there's no information leak before verification. The verify gate holds up. On the JWT replay angle, capturing a provider JWT and replaying it with an attacker's device pubkey would enroll that rogue device into the victim's account, but that requires breaking TLS or a compromised client, and the enrolled device still can't route calls without verification. This falls under the stolen-provider-account threat model, which is already covered.

I should note that the enrollment JWT should be short-lived and single-use to mitigate replay risk, though binding it to the device pubkey isn't feasible since the device chooses its own key. Now I'm pulling together the final writeup with concrete section references and dropping the raw tool outputs.
I've read both documents fully and verified the load-bearing inheritance claims against the phase-2 text. Let me confirm the phase-2 partition/keepalive and topology semantics that phase-3 claims to inherit.

[assistant] 2026-07-07T12:07:02Z
I'm identifying some critical inconsistencies in the phase-2 documentation. The phase-3 claim that relay-path partitions classify identically to TCP partitions contradicts phase-2's assertion that the reaper serves as the authoritative partition classifier through keepalive monitoring, and the phase-3 description of relay idle-teardown as partition-like behavior seems to conflate different mechanisms.

There's a race condition I'm concerned about: if the relay's idle timeout fires faster than the keepalive cadence, the pipe tears down between keepalives and looks like a partition to the reaper, but this is actually a new failure mode that phase-2 never addressed. The phase-3 documentation asserts the equivalence without accounting for this scenario.

I'm also noticing a potential issue with noise session resumption and the reaper's staleness detection. The incarnation UUID is tied to the send-log database rather than the session itself, so it persists across re-handshakes, but if a re-handshake stalls and the reaper's staleness window is tight, it could trigger a spurious partition detection and send an OutcomeUnknown response for in-flight calls.

More critically, there's a gap in the recovery logic: phase-2 specifies recovery reconciliation only on fed-module restart, but a relay reconnect mid-effect doesn't trigger a restart—the fed-module is still running, just without transport. So when the pipe tears down and reconnects, the origin needs to re-query the serving ledger for in-flight effects whose sends were written but outcomes weren't received. Phase-3 claims this works correctly but doesn't specify what triggers the reconciliation on a mid-session reconnect, which is a real gap compared to phase-2's restart-triggered recovery.

On the pipe token design, there's an internal inconsistency: phase-3 §5.3 shows a single `pipe_token` in the relay grant message, but §7 says "per-side `pipe_token`"—unclear whether it's symmetric or per-side. Single-use enforcement is also underspecified; the token gets consumed on connect, so reconnects after idle teardown need a fresh grant, but the atomic check-and-consume location isn't defined. It happens in the RelayDO, which is single-threaded so it's safe, but this is a medium-level finding on the inconsistency between §5.3 and §7.

Now looking at the candidate racing problem: both sides race candidates simultaneously, but the pubkey tie-break is supposed to select a single initiator. The issue is that in phase-3, both devices see each other in the registry at the same time and can independently initiate connect_request, creating a simultaneous-open scenario where both sides dial each other's public candidates, resulting in two TCP connections and two Noise sessions. The phase-2 tie-break resolves who connects in static profiles, but it's unclear whether it actually covers the symmetric discovery case where both sides learn of each other and both initiate independently.

The spec says the tie-break picks the initiator, but the language about "both sides racing candidates" is ambiguous—it might mean both sides race candidates only on the relay path where they dial out together, but this isn't explicit. Additionally, if a TCP session from a static address candidate establishes at the same time as a relay session, the first-success-wins rule only applies per-initiator, not globally, so a double-win is still possible if the tie-break isn't perfectly enforced during the discovery window. The real fix needs explicit deterministic loser-teardown rather than relying on first-success-wins alone.

On the WebSocket framing side, the spec says one fed record per WS message with a retained 4-byte length prefix that's technically redundant. The concern is what happens if a malicious relay or a bug sends a WS message with a mismatched number of records or a length prefix that disagrees with the actual message length.  Since Noise authenticates the ciphertext, a tampered length can't inject content, but a mismatch could cause parsing divergence—the TCP parser expects to read a stream and might block waiting for more bytes if the prefix claims more data than the message contains, or the carrier would need to buffer across messages, defeating the whole point of the "one record per message" invariant. The spec needs to explicitly require the receiver to validate that the WS message length equals 4 plus the prefix value and contains exactly one record, rejecting any mismatch by closing the session rather than feeding it into a stream parser. This is a real correctness issue, and it's still unclear whether the length prefix sits inside or outside the ciphertext.

Signaling messages lack nonces or timestamps, which means a compromised relay can replay them—though since signaling carries no secrets and Noise IK gates actual routability, the impact is mostly DoS: a replayed connect_offer wastes a dial attempt, and a replayed relay_grant with a single-use, TTL'd token is bounded. But replayed connect_requests could flood the target with connect_offers.

The real gap: no rate limiting on signaling in phase-3 (phase-2 had relay quotas, but phase-3 doesn't specify per-device limits), so a stolen device token lets an attacker flood connect_requests and poison candidates of that device's own entry—a MEDIUM finding.

On candidate poisoning, a sibling device can report a LAN candidate pointing at an arbitrary IP:port (SSRF-style), forcing peers to send connection attempts there, but the Noise IK handshake fails when the target presents a different key, so the peer falls back to the next candidate—no MITM, but it's a DoS and port-scan amplification vector.  The cloud can also inject any candidate it relays, turning the rendezvous-driven dialer into a confused deputy hitting internal addresses; the fix is to validate candidates against expected ranges, cap dial attempts, and reject non-routable/loopback/link-local addresses unless they're LAN candidates in a private range matching the reporter's subnet—MEDIUM severity bounded by same-account scope.

For the revoked_by_account re-pairing path, when a device is removed and the same pubkey re-enrolls (whether a stolen key or a reinstall), the doc says re-pairing is required, but there's a race condition: nothing explicitly enforces that peers can't auto-trust a re-enrolled key without redoing the verify-code handshake.

The real issue is that a revoked pubkey materializes as unverified in discovery, which is fine for first-contact, but an attacker who stole the private key could re-enroll and trick the user into re-verifying—revocation should be stickier than just "unverified," either by blocklisting the key or requiring stronger confirmation than a normal pairing ceremony. This is MEDIUM-HIGH because private key theft combined with re-enrollment and social engineering defeats the revocation model.

Now I'm looking at enrollment races: if two devices try to enroll with the same pubkey, the DO needs to enforce uniqueness atomically—either via upsert or rejection—but the doc doesn't specify a UNIQUE constraint on device_pubkey within an account. Cross-account pubkey collisions are probably isolated by design, though there's potential for registry ambiguity if not handled carefully.

Account-id resolution is a bigger concern: concurrent first-logins for the same provider subject could mint duplicate account_ids if the resolution happens in the stateless Worker rather than through a subject-keyed Durable Object. The doc says the Worker "resolves/creates account_id" but that's a race condition unless serialized through a DO or enforced with a D1 unique constraint—this is a medium-high finding.

For rendezvous degradation, I'm checking whether a present-but-down rendezvous cleanly falls back or hangs the peer. The static address is listed as a fallback candidate, so the question is whether all paths avoid blocking when rendezvous is unavailable. The doc needs to explicitly state that all security-relevant TTLs—token expiry, pipe_token TTL, candidate freshness—are evaluated server-side only, never against device wall-clock, since device clock skew could either brick auth or allow expired tokens. On the WS hibernation front, cold-waking the AccountDO on first signal could delay the connect_request→offer→accept signaling round-trip, but since no deadline is specified for connect_request itself, the risk is mostly mitigated unless a deadline gets added later.

The bigger operational concern is the single DO per account acting as a chokepoint: a busy account with many devices or a signaling flood from a compromised token concentrates all load on one DO in one region, degrading service for all that account's devices and creating cross-region latency for globally distributed users. The finding is to document per-account DO capacity limits and rate-limit signaling per device to prevent one flooding device from poisoning the whole account.

Now I'm examining the pairing UX attack surface: the cloud can offer a rogue device during the legitimate pairing window, and if the user is confused or the cloud suppresses the real device's join event and substitutes the rogue's, the verify-code comparison becomes unreliable. The code is derived from both static keys in an order-independent way, so if the user accidentally pairs device A with the rogue (thinking it's their real new device B), both sides display the same code and match — the user gets tricked into confirming the wrong pairing.

The defense depends entirely on the user comparing the code against the physical device they actually intend to pair with; if the cloud never delivers the real device into the flow, the user comparing A's screen against the physical device in front of them will see a mismatch and reject. Phase-3's "loud offer-to-unverified" worsens this by enabling notification fatigue and confusion attacks — rogue devices can spam pairing offers until the user mis-taps one.

The fix is to bind the code prompt to a user-initiated pairing action (scanning or entering something on the target device) rather than to a cloud-pushed join event, and gate unsolicited offers to verified targets except during an explicit pairing session. The strongest approach is to require verified pairing unless a short-lived "pairing mode" is active on the target device, which eliminates unsolicited spam while preserving the pairing flow.

Now examining the relay grant mechanics — when device X requests a relay to Y, the rendezvous issues a grant to both sides, but Y never explicitly consents. An attacker with a stolen token can relay_open to any pubkey in the account, forcing a grant to be pushed to Y and causing resource consumption on both Y and the relay; without rate limits, this becomes a relay-open flood. Cross-account relay_open is blocked by the Durable Object isolation, but the pipe_token redemption creates an inconsistency: if the grant uses a symmetric token (one token for both sides), device X could redeem both sides and deny Y its slot, whereas per-side tokens prevent this. Single-use tokens with short TTLs help bound the abuse, but the symmetric vs. per-side design inconsistency is the core issue.

Now I'm looking at the effect-ledger over-reconnect scenario more carefully since it could be a blocker that corrupts effects and loses mutations. The origin logs intents and outcomes durably before sending and before the consumer sees the reply, and never auto-retries post-send on the WAN. On restart, the origin queries the serving ledger for sent-without-outcome effects. But if the relay idle-teardown or a transient reconnect drops the pipe mid-effect while the origin fed-module is still running (no restart), the origin has the effect in sent state with no outcome. Since there's no restart, the reconciliation query doesn't fire, and the effect hangs indefinitely in OutcomeUnknown.

This means a relay reconnect mid-effect stalls the effect's resolution — the outcome exists on the serving side but the origin never re-queries because reconnect isn't treated as a restart trigger. For a mutating call, this results in the mutation's result being marked unknown and unretryable, so the user doesn't know if it happened. It's not corruption (at-most-once holds), but it's a correctness gap in mutation outcome delivery. The phase-3 claim about recovery reconciliation across a new pipe isn't supported by phase-2 since reconciliation is only restart-triggered, not transport-reconnect-triggered. I'm rating this HIGH and recommending that reconnect must trigger the same reconciliation — re-query the serving ledger for all sent-without-outcome effects over the new pipe, making reconciliation transport-event-triggered rather than just restart-triggered.

There's also a critical interaction with the serving ledger's retention window. The dedup ledger is the safety net if a naive implementation re-sends on reconnect, but the outcome is retained only until the origin confirms receipt. If the origin never receives the outcome because it's not re-querying, the ledger row stays retained, but if the grace period expires while the origin is disconnected, a later reconnect and requery could hit an expired outcome — the mutation outcome is permanently lost to the caller. This is HIGH because relay idle-teardown windows can be long (device offline) and the ledger grace could expire during that time.

Now I'm examining the keepalive-versus-idle-teardown partition finding. If keepalive traffic flows at cadence C, the pipe should stay warm and the relay idle-teardown shouldn't fire during an active session — assuming the relay idle timeout exceeds the keepalive cadence. But if the relay idle timeout is shorter than the keepalive cadence, the relay tears down between keepalives, the next keepalive fails, the reaper counts a miss, and after three misses it declares a partition and closes the loopback.

Actually, keepalives do traverse the pipe as fed-frames, so they reset the relay's idle timer — meaning idle-teardown only fires when both sides are truly idle. Since keepalive is periodic regardless of app traffic, the pipe should never idle as long as both fed-modules are alive and keepalive-ing. So idle-teardown would only fire when a fed-module stops keepalive-ing, which means it's actually gone — a real partition. But here's the tension: the whole point of relay idle-teardown is to save relay cost when the pipe is idle. If keepalives keep it perpetually warm, the pipe never idles and you pay for the relay continuously, defeating the cost optimization. So either keepalives keep the pipe warm and you lose idle savings, or keepalives are suppressed to allow idling but then the reaper's staleness window would trip on the idle gap.

The interaction between relay idle-teardown, keepalive cadence, and the 3× reap window is fundamentally underspecified and creates three possible failure modes: false-positive partitions if reconnect latency exceeds the reap window, no cost savings if keepalives keep the pipe warm, or constant re-handshake churn if the pipe idles between keepalives. The critical difference is that TCP partitions don't have this idle-teardown-then-reconnect middle state — the relay introduces a new "transport gone but peer alive" state that TCP doesn't have, so the reaper's classification of "missed keepalive = partition" can't actually distinguish between a real partition and a relay reconnect in progress.

The concrete fix requires either resetting the relay idle timer on every keepalive and setting the idle timeout beyond the reap window (accepting continuous relay cost), or introducing an explicit transport-reconnect state where the reaper suspends the reap timer during reconnect and only escalates to partition after a separate bounded reconnect deadline. This is a high-severity finding because spurious GOODBYE on an in-flight mutation call leads to OutcomeUnknown, corrupting the UX contract.

On the Noise session resumption claim: re-handshakes mint new sessions but don't change the effect_id incarnation since that's tied to the send-log DB, not the session, so dedup continuity holds. However, "cheap" is misleading — a Noise IK re-handshake over a relay requires two round-trips through the relay plus a fresh pipe grant, which adds non-trivial latency.

More critically, re-handshakes create a new Noise session with fresh nonces, so any fed-frames in flight during teardown are lost since Noise/TCP don't guarantee delivery across session boundaries. This means an effect whose bytes were mid-flight when the pipe tore down may be partially sent, leaving the effect in `sent` state at the origin but never fully arriving at the serving side — requiring reconciliation on reconnect.

The relay-path partition classification claim is also false: idle-teardown shows partitions aren't equivalent to TCP partitions, and WebSocket carriers over relay (which restart more frequently due to hibernation eviction or CF maintenance) create more common partition events, reinforcing spurious classification. Now I need to verify the pubkey tie-break double-win claim and check whether phase-2 specifies a simultaneous connection tie-break. The concrete fix is to enforce a deterministic initiator based on lexicographically-lower pubkey, maintain a single-session invariant per peer at the fed-module level by deduplicating concurrent establishments and tearing down the redundant one before re-HELLO, and make phase-2's tie-break mechanism explicit in the documentation. I'm also verifying the other inheritance claims—the verified-gate making discovered peers non-routable does hold in phase-2, and the reaping behavior is documented there too.

Now I'm checking the remaining details: the 16 MiB frame cap is consistent between phases, but I've found another issue with the device token. It's a pure bearer token over TLS with no proof-of-possession binding, so stealing the 0600 file grants full signaling impersonation—an attacker can poison the victim's registry candidates and flood without needing the private key. The fix is to bind the token to the device static key via a signature, making file theft alone insufficient. This directly answers Q1 and is medium-high severity.

For the reconnect-mid-effect gap, I'm wrestling with the severity rating. The mutation itself executes and stays in the ledger, but the outcome visibility is lost if the origin never re-queries before the grace period expires—marking it unknown to the caller. Phase-2's entire recovery reconciliation was designed to close exactly this window, and phase-3's new transport-reconnect trigger reopens it without restart. That's a regression of a shipped blocker-level fix, so I'm rating this as blocker despite the mutation technically surviving in the ledger.

Now looking at the other issues: the double-win scenario with missing phase-2 tie-break can trigger eviction storms that lose mutation outcomes on mutators, which I'm flagging as high with blocker leanings. The rendezvous-present-but-down case bricks static-reachable peers if dialing is signaling-gated, breaking the airtight fallback requirement—that's high. Keepalive and idle-teardown spurious partitions are high. The WebSocket framing mismatch is medium since Noise AEAD contains the blast. Device bearer tokens lacking proof-of-possession enable signaling takeover and candidate poisoning, so that's high. Account-id creation races and enrollment pubkey uniqueness races are medium. Revoked pubkey re-enrollment not being blocklisted is medium—the ceremony still gates routability, but revocation should be sticky. And I'm starting to look at candidate SSRF and port issues.

Continuing through the remaining findings: SSRF via poisoned LAN addresses is medium, as are signaling floods without rate limits or replay protection. The pipe_token symmetric-versus-per-side inconsistency and redemption authz gaps are medium. DO-per-account hot spots and flood amplification are medium. Pairing confusion from loud offers to unverified devices is medium. Device clock and TTL trust issues drop to low. WebSocket hibernation cold-wake versus timeout handling is low-medium, though that's acceptable with decoupled deadlines.

For the verdict: I'm going with GO-WITH-CHANGES, but I need to be disciplined about what's truly a blocker versus a high must-fix. F1—reconnect-triggered reconciliation—is the only genuine blocker by the stated bar since it reopens a closed effect-durability hole. F2 through F5 (explicit deterministic initiator with single-session invariant, rendezvous-down static fallback decoupled from signaling, keepalive and idle-teardown partition disambiguation, and proof-of-possession device token) are all high must-fixes before build. I'll present that distinction clearly.
I've read both documents fully and cross-checked every inheritance claim against the phase-2 (v4.1) text. Here is my adversarial review.

<COUNCIL_MEMBER_RESPONSE>

# ADVERSARIAL REVIEW — subc Federation Phase 3 (DRAFT v1)

Cross-checked against `subc-federation-design.md` v4.1. Findings ranked by honest severity; only one true BLOCKER.

---

## F1 — Relay reconnect mid-effect RE-OPENS the closed at-most-once recovery hole
- **Severity**: BLOCKER
- **Section**:  ("idle timeout tears the pipe down… Noise session resumption = ordinary re-handshake, cheap"), inheriting phase-2 6.1.
- **Problem** (step by step):
  1. Origin sends effect E over the relay pipe. Phase-2 6.1 durably records `intent → sent` (fsynced), then, per its own rule, **"NEVER auto-retries post-send on the WAN."**
  2. Before the outcome returns, RelayDO idle-teardown (or CF hibernation/DO eviction/transient WS close) drops the pipe. **The origin fed-module does NOT restart** — only the transport died.
  3. Phase-2's recovery reconciliation (6.1, the mechanism that closed BLOCKER #3's pre-outcome window) is **RESTART-triggered only** — "on restart the origin fed-module queries the serving ledger." No restart ⇒ the query never fires.
  4. Effect E is now stuck in `sent`/`OutcomeUnknown` on the origin, even though the peer may have executed it and the outcome is sitting in the serving dedup ledger. For a `mutating` op, 2.3 forbids auto-retry ⇒ the outcome is **lost to the caller** (marked unknown).
  5. Worse: serving-ledger retention (6.1 v4) is "retained until origin CONFIRMS outcome-received + grace." The origin can't confirm because it isn't re-querying; if the grace expires while disconnected, a later re-query hits `effect_outcome_expired` ⇒ the mutation result is **permanently unrecoverable**.
- **Inheritance verdict**: The phase-3 claim that "recovery reconciliation interacts correctly across a new pipe" is **NOT supported by phase-2**. Phase-2 6.1 defines the reconciliation trigger as *restart*, never *transport reconnect*. Relay idle-teardown introduces a reconnect-without-restart state that phase-2 never contemplated, re-opening the exact window BLOCKER #3 was declared to close.
- **Fix**: Make reconciliation **transport-event-triggered, not restart-triggered.** On every carrier reconnect (relay or TCP), the origin fed-module MUST replay the 6.1 recovery query over the new pipe for every `sent`-without-outcome effect_id before resuming normal traffic. Additionally, gate serving-ledger grace expiry on *elapsed reachable time*, not wall-clock, so a long relay outage cannot expire an unconfirmed row.

---

## F2 — Symmetric discovery creates a double-win; the cited "phase-2 pubkey tie-break" does not exist in phase-2
- **Severity**: HIGH
- **Section**: 5.3–5.4 ("the initiator (pubkey tie-break as phase 2)"; "both sides then race candidates").
- **Problem**: Phase-2 designated connection direction by *reachability* (one side has a static addr and dials; the other listens). Phase-3 makes discovery **symmetric** — both devices see each other in `registry_snapshot` on `hello` and either can send `connect_request`. If both do (a routine race the moment both come online), you get two independent establishments: A dials B's public candidate AND B dials A's public candidate ⇒ two Noise sessions. Phase-2 2.5 opens one loopback connection per (peer, remote module) and `register_module_connection` **evicts the prior registration** — so the second session's HELLO evicts the first, killing every in-flight call on the first session (deterministic GOODBYE ⇒ `OutcomeUnknown` on in-flight mutators). A TCP-candidate win racing a relay/WS win produces the same collision across carriers.
- **Inheritance verdict**: **Phase-2 does NOT specify a pubkey tie-break for simultaneous open** (grep of the v4.1 doc: no "tie-break"/"initiator"/"simultaneous" mechanism anywhere). The phrase "as phase 2" cites a mechanism that isn't in phase-2. This is an invented inheritance.
- **Fix**: Define the tie-break explicitly in phase-3: the **lexicographically-lower pubkey is the sole initiator**; the higher pubkey listens and MUST NOT send `connect_request`. Enforce a **single-session-per-peer invariant** at the fed-module: if two establishments race to completion, deterministically keep the initiator's and tear down the other *before* the redundant re-HELLO can evict a live connection.

---

## F3 — Rendezvous present-but-down can brick an otherwise static-reachable peer if dialing is signaling-gated
- **Severity**: HIGH
- **Section**: 5.5 (`[rendezvous]`-absent = phase-2; static addr = "one more candidate, highest priority").
- **Problem**: The requirement is airtight degradation. `[rendezvous]`-absent is fine. But **present-but-down** is only safe if the dial flow can start *without* rendezvous signaling. As written, dialing is triggered by `connect_offer/accept` (5.3–5.4). If a peer has a perfectly good static `addr` candidate but the control WS is down/hung, and dialing only begins on receipt of a `connect_offer`, then a rendezvous outage makes an otherwise-static-reachable peer **unreachable** — the failure the fallback is supposed to prevent. The doc never states that static candidates are dialed on a path independent of the signaling round-trip, nor bounds the control-WS connect timeout.
- **Fix**: Specify a **signaling-independent direct-dial path**: any peer with a static/last-known candidate is dialed immediately on startup regardless of control-WS state; the control WS has a bounded connect timeout (e.g. 2–3s) after which the module proceeds on static candidates and retries rendezvous in the background. Add an acceptance test: kill the rendezvous mid-run and assert a static-reachable peer stays connected.

---

## F4 — Keepalive vs relay idle-teardown: false-positive partitions; "classifies exactly like TCP" is false
- **Severity**: HIGH
- **Section**:  ("keepalive cadence and 3× reap window unchanged; relay-path partitions classify exactly like TCP partitions") vs  (idle-teardown) + Open Q3.
- **Problem**: These three cannot all hold. TCP has no "transport gone but peer alive" middle state; the relay's idle-teardown creates exactly that. Two irreconcilable regimes:
  - If keepalives traverse the pipe at cadence, they reset the relay idle timer ⇒ the pipe **never idles** ⇒ no cost savings (contradicts Q3's premise of reconnect-per-idle-teardown churn).
  - If keepalives are relaxed to let the pipe idle (Q3's churn model), the idle gap + cold RelayDO wake can exceed the 3× reap window ⇒ the reaper (phase-2 6.2, the *authoritative* partition classifier) declares a **spurious partition**, closes the loopback, and fires GOODBYE ⇒ `OutcomeUnknown` on in-flight mutators — a false positive on a live peer.
  The reaper "missed keepalive = partition" logic (phase-2 6.2) **cannot distinguish** "relay idled the pipe" from "peer died." So the inheritance claim is false.
- **Fix**: Introduce an explicit **transport-reconnecting state** distinct from partition. The reaper suspends its reap timer while a carrier reconnect is in progress and escalates to partition only after a *separate* bounded reconnect deadline. Constrain: relay idle-timeout > (3× keepalive cadence + worst-case cold-wake), OR bill continuous relay time and never idle an active session. Pair with F1's reconnect-triggered reconciliation.

---

## F5 — Device token is a pure bearer with no proof-of-possession
- **Severity**: HIGH
- **Section**: 4.2 ("opaque bearer bound to (account_id, device_pubkey)… stored hashed"), Open Q1.
- **Problem**: A bearer token means possession = full signaling identity of that device. Theft of the `0600` file alone (no private key needed) lets an attacker: rewrite the victim's own registry candidates (redirect the victim's inbound dials to attacker IPs — DoS), flood `connect_request`/`relay_open`, and consume the victim's slot. Yes, it can't speak Noise (F5 mitigation in  so no routability — but the signaling-plane takeover is real and, over the theft window, damaging. The token is not bound to the device static key by any signature.
- **Fix**: Make it a **proof-of-possession** token: the WS auth challenge is signed with the device Noise static key (the token names the device; the signature proves custody). File theft without the private key then yields nothing. This directly answers Q1: `0600` is **not** sufficient with bearer semantics.

---

## F6 — WS framing: length-prefix / message-boundary disagreement is unspecified
- **Severity**: MEDIUM
- **Section**:  ("one fed record per WS message; the 4-byte length prefix… redundant but kept identical so record parsing is carrier-agnostic").
- **Problem**: The length prefix must be plaintext (it frames the ciphertext), so a malicious/buggy relay can flip it. "Kept identical so the same parser runs" is the hazard: the TCP parser is a *stream* parser (read prefix → read N bytes, possibly spanning segments). If the WsCarrier feeds it one WS message whose prefix says N but the message body is M≠N, either (a) the stream parser blocks awaiting bytes that will never come in this message, or (b) the carrier must buffer across messages — re-introducing the exact framing ambiguity "one record per message" was meant to remove. Noise AEAD contains the blast to a decode failure (no content injection), but the result is session teardown / DoS and undefined buffering.
- **Fix**: The WsCarrier receiver MUST assert `ws_message_len == 4 + prefix_value` and that exactly one record is present, and **reject-and-close on mismatch** — never feed a length-inconsistent message into the stream parser. State this as a normative invariant with a golden negative-vector.

---

## F7 — Candidate self-report enables SSRF / internal port-scan via a sibling or the cloud
- **Severity**: MEDIUM
- **Section**: 5.2 (LAN candidate self-reported), 4.3 (cloud relays offers), Open Q2.
- **Problem**: Q2's answer ("can only redirect its own inbound dials; Noise pins identity") is incomplete. A malicious sibling (or a compromised cloud injecting a `connect_offer`) can advertise a LAN candidate pointing at an arbitrary internal `ip:port` on the *victim dialer's* network. The dialer performs a TCP connect (and handshake attempt) before Noise fails on key mismatch — turning the rendezvous-driven dialer into a **confused-deputy port scanner** hitting internal addresses chosen by an attacker. Noise stops MITM, not the connect itself.
- **Fix**: Validate candidate addrs before dialing: reject loopback/link-local/multicast; require LAN candidates to fall within the dialer's own observed private subnet; cap concurrent dial attempts and per-target retries. Never dial an addr class inconsistent with the candidate `kind`.

---

## F8 — Account-id creation and pubkey enrollment lack specified atomicity
- **Severity**: MEDIUM
- **Section**: 4.2 (steps 2–3: Worker "resolves/creates account_id (keyed by provider subject)"; DO records device).
- **Problem**: (a) **Account split-brain**: Cloudflare Workers are stateless and horizontally scaled. Two concurrent first-logins for the same provider subject can both "create" an account_id ⇒ duplicate accounts / partitioned device graph. Resolution in the stateless Worker is a race. (b) **Duplicate pubkey enrollment**: no stated UNIQUE constraint on `device_pubkey` within an account; two enrolls for one pubkey can mint two tokens/records with ambiguous registry identity.
- **Fix**: Serialize account creation through a **DO keyed by hash(provider subject)** (or a D1 unique index) so resolution is atomic. Enforce a unique `(account_id, device_pubkey)` constraint in the AccountDO with atomic upsert-or-reject.

---

## F9 — Revocation is not sticky against re-enrollment of the same pubkey
- **Severity**: MEDIUM
- **Section**: 4.2 ("`revoked_by_account`, requires re-pair if re-enrolled"), phase-2 6.4 (signed revocation/tombstone).
- **Problem**: `revoked_by_account` flips a device to non-routable, but 5.5 says a re-appearing pubkey **materializes as an ordinary unverified peer** (`verified:false`). There is no distinction between "never seen" and "explicitly revoked." If an attacker who obtained a device's private key re-enrolls the same pubkey, its verify-code *matches* (same key) — a user re-verifying after a "device rejoined" notification silently re-trusts the attacker. Revocation should tombstone the pubkey, not merely reset it to first-contact.
- **Fix**: Persist a per-account **revocation tombstone** for the pubkey (inheriting phase-2 6.4's signed tombstone). A tombstoned pubkey re-appearing requires an explicit, distinct "previously-removed device is re-enrolling — confirm you intend this" flow, not the ordinary quiet first-contact path.

---

## F10 — Signaling has no rate limiting or replay protection
- **Severity**: MEDIUM
- **Section**: 5.3 (ops list — no nonce/timestamp/sequence; no per-device rate limit).
- **Problem**: Phase-2 5.5 gave the relay explicit "per-account/per-device quotas + rate limits"; phase-3 signaling (`connect_request`, `relay_open`, candidate updates) specifies **none**. A stolen device token (F5) or a compromised cloud can flood `connect_request` ⇒ target `connect_offer` spam ⇒ wasted dials, and `relay_open` floods ⇒ forced relay grants pushed to victims ⇒ resource consumption. No nonce/timestamp means replays of stale signaling are indistinguishable (low trust impact since signaling carries no secrets, but a DoS multiplier).
- **Fix**: Per-device token-bucket rate limits on all signaling ops in the AccountDO; monotonically increasing per-connection sequence numbers on signaling messages with stale-drop. Require the target's implicit consent for `relay_open` (bounded pending grants per target).

---

## F11 — `pipe_token`: 5.3 (single symmetric token) contradicts  (per-side); redemption authz unspecified
- **Severity**: MEDIUM
- **Section**: 5.3 (`relay_grant {relay_url, pipe_id, pipe_token}` "issued to both sides") vs  ("per-side `pipe_token`, single-use, short TTL").
- **Problem**: The wire schema shows ONE `pipe_token`;  says per-side. If symmetric, one party (or a token leak) could redeem **both** sides of the pipe — occupy both ends, deny the peer its slot, or connect the relay to itself. "Single-use" enforcement location, and whether device X can redeem device Y's grant, are unspecified.
- **Fix**: Commit to **per-side tokens**: `relay_grant` carries a side-scoped `pipe_token_self`; RelayDO binds each token to a side and to the redeeming device's authenticated identity, enforces single-use via atomic check-and-consume (DO single-threaded execution makes this clean), and rejects a token redeemed for the wrong side or by the wrong device. Fix the 5.3 schema to match.

---

## F12 — One DO per account is a single-threaded chokepoint and DoS-amplification point
- **Severity**: MEDIUM (operational)
- **Section**: 5.1 (AccountDO one-per-account),  Q-implicit.
- **Problem**: All signaling + registry for an account funnels through one single-threaded DO pinned to one region. One misbehaving/flooding device (F10) degrades *every* device on that account. Geographically dispersed devices on one account pay cross-region latency to the DO home.
- **Fix**: Document per-account DO throughput limits; enforce per-device fairness (F10) so one device cannot starve siblings; consider a signaling-only sub-DO shard for high-fanout accounts. Not gating for a few-device personal account, but must be bounded before paid-tier scale.

---

## F13 — "Loud offer-to-unverified" plus cloud-controlled join events enables pairing-confusion / fatigue
- **Severity**: MEDIUM
- **Section**: 4.3,  (cloud gains "rogue pairing offers"), Open Q4.
- **Problem**: The verify-code defense (phase-2 5.3, correctly inherited) holds *only if the user compares the code against the physical device they intend to pair*. A compromised cloud that both injects a rogue device AND controls which join events/codes surface can drive a **confusion attack**: a cloud-pushed "device joined" prompt piggybacks a code comparison the user didn't initiate, and notification fatigue from loud unsolicited offers increases mis-taps. The order-independent safety number matches between whatever two keys are actually being compared, so the security rests entirely on the human comparing against the right screen.
- **Fix (also answers Q4)**: Do **not** relay `connect_offer` to an unverified target by default. Require an explicit, short-lived **user-initiated pairing window** on the target device; only during that window is offer-to-unverified permitted. Bind the code prompt to the user's local pairing action, never to a cloud-pushed join event.

---

## F14 — Wall-clock trust in TTL/last_seen/token-expiry is not pinned to server time
- **Severity**: LOW
- **Section**: 5.2 (`last_seen_ms`, TTLs), 4.2 (token expiry),  (pipe_token TTL).
- **Problem**: The doc doesn't state that all security-relevant TTLs are evaluated against **server** time. A device with a skewed/rolled-back clock that self-evaluates token expiry could either keep using an "expired" token (server rejects — benign) or discard a valid token (self-DoS). Device-reported `last_seen_ms` from a skewed clock poisons ordering.
- **Fix**: State normatively that token expiry, pipe_token TTL, and candidate freshness are enforced **server-side only**; `last_seen_ms` is server-stamped; devices never make security decisions on local wall-clock.

---

# ANSWERS TO THE FIVE OPEN QUESTIONS

1. **Device-token custody on disk.** `0600` is **insufficient as specified**, but the real fix isn't the vault — it's F5: change the token from a bearer to a **proof-of-possession** credential signed by the device Noise static key. Then file theft alone grants nothing, and `0600` next to the (equally sensitive) private key is fine. Fold into the credentials vault only if the private key already lives there; otherwise co-locating with the device key at `0600` + PoP is adequate for v1.

2. **Registry candidate self-reporting.** Hardening **is** needed (F7). The "only redirects its own inbound" claim ignores that the *dialer* is the victim of an SSRF/port-scan. Require server-side sanity checks: LAN candidates must fall in the reporter's observed private subnet; reject loopback/link-local; cap dial fan-out. The Noise-pins-identity argument is correct for MITM but irrelevant to the connect-side abuse.

3. **Relay pipe lifetime.** Per-connection grants with idle-teardown are acceptable **only after F1 and F4 are fixed**. As drafted, idle-teardown reopens the effect-reconciliation hole and confuses the partition classifier. Recommendation: **standing pipe per active peer-pair** kept warm by keepalive while a session is live (bill the relay time — it's the paid-tier meter anyway), and idle-teardown only after the fed session itself goes fully idle beyond the reap window, with an explicit reconnect state. Reconnect-per-keepalive churn is **not** acceptable.

4. **Require verified before signaling?** **Yes, with an exception window** (F13). Default: `connect_offer` is relayed only to verified targets — quiet, no unsolicited pairing spam. Permit offer-to-unverified **only** inside a short, user-initiated pairing window on the target. This preserves the pairing UX while killing the cloud-driven confusion/fatigue surface. Current "always allowed, loud" is the wrong default.

5. **WS hibernation cold-wake.** Acceptable to pay a cold-wake round-trip on **first signal**, provided signaling deadlines are **decoupled from the ~2s data-candidate timeout** (F-note). Give first-signal a generous deadline (~10s) and warm the target DO on the initiator's `connect_request`; never let cold-wake latency count against the per-candidate dial budget or a `connect_request` deadline. Measure p99 wake in the 3b drill.

---

# OVERALL VERDICT: **GO-WITH-CHANGES**

The architecture is sound and the E2E/verify-code trust core (correctly inherited from phase-2 2.2/5.3) genuinely keeps the cloud and relay untrusted-by-construction — no cross-account routability escalation survives scrutiny. But two inheritance claims are **not actually supported by phase-2** (F1 restart-only reconciliation; F2 nonexistent tie-break) and one is **false** (F4 relay ≠ TCP partition), and these interact to threaten effect durability.

**Must-fix before build:**
- **F1 (BLOCKER)** — reconnect-triggered reconciliation; without it, a relay idle-teardown reopens the closed at-most-once outcome-loss window (a regression of shipped BLOCKER #3).
- **F2 (HIGH)** — specify the deterministic single-initiator tie-break + single-session-per-peer invariant; "as phase 2" is not in phase-2.
- **F3 (HIGH)** — decouple static-candidate dialing from rendezvous signaling; prove the outage-fallback is airtight.
- **F4 (HIGH)** — explicit transport-reconnecting state so idle-teardown ≠ spurious partition; retract "classifies exactly like TCP."
- **F5 (HIGH)** — proof-of-possession device token, not bearer.

F6–F13 are real gaps to schedule within the phase; F14 is polish. Confidence: **high** on F1/F2/F4 (direct textual contradiction with phase-2), **high** on F3/F5, **medium** on F7/F8/F9/F13 (attack is clear; exact blast radius depends on unspecified implementation). No wall of blockers — one true BLOCKER, four HIGH must-fixes, the rest containable.

</COUNCIL_MEMBER_RESPONSE>

