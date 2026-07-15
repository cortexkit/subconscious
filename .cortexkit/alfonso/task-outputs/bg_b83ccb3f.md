---
task_id: bg_b83ccb3f
agent: council-member
session_id: ses_0ce292390ffegxXVcaaSrNh30Q
parent_session_id: ses_0ce29eec3ffej0uyjk5swjgaZj
status: completed
completed_at: 2026-07-05T10:40:52Z
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
I'm going to read through the design doc and synthesis file, then map out the source files I need to cross-reference.
I'll start by reading the two required files in full and pulling the source files I need to verify against.

[assistant] 2026-07-05T10:33:22Z
I'm spotting a contradiction between the v4 design doc and an earlier finding—the spec mentions tools getting a route-GOODBYE on removal, but that conflicts with a previous note that said there shouldn't be tool-granular GOODBYE messages. Let me trace through the details more carefully to understand what's actually specified. Now I need to check those additional source files across the codebase to verify whether the folds in v4 actually match what's in the real implementation.
Both files are loaded. I've already spotted a candidate cross-section contradiction (4.1 line 130 vs 2.6 line 85 on GOODBYE). Now let me verify the folds against the actual source lines cited.

[assistant] 2026-07-05T10:33:41Z
I need to correct my file location—the manifest.rs is in subc-protocol, not subc-core. Let me verify the source line references I mentioned: control.rs has manifest_concurrency around line 619 and effective_module_control_ops near 584, health prober at 1260; supervise.rs has reserved_hello_authorized at lines 384-395 and SUBC_LAUNCH_NONCE_ENV at 2033; forwarding.rs has register_module_connection with eviction logic around line 280.

I'm confirming several key findings: the DuplicateModuleId rejection in registry.rs matches the v3 finding, the ModuleRouteKey and RouteBinding structures in forwarding.rs show that routes are scoped to module and channel without carrying tool identity, and the GoodbyeTargetKind Module variant confirms best-effort DROP behavior without closing connections. Now I need to examine the protocol manifest and trace through those other source locations.
The manifest is in subc-protocol. Let me read the correct path plus the remaining source lines.

[assistant] 2026-07-05T10:33:53Z
Checking the specific line numbers across control.rs and forwarding.rs to verify the implementation details around concurrency handling and module operations.
Now let me confirm the control.rs pieces (#2) and the launch nonce env (#3).

[assistant] 2026-07-05T10:34:04Z
I'm finding a contradiction in the v4 spec between section 2.6, which states there's no tool-granular GOODBYE handling on the module side, and section 4.1, which says removed tools do get a route-GOODBYE. Let me reread line 130 to understand what's actually being specified there.

The catalog update section explicitly says removed tools receive route-GOODBYE, which directly contradicts the earlier statement that calls to removed tools reach the module and get a module-side typed error instead. This is a real inconsistency—either line 130 is a leftover from v3 or it represents a genuine contradiction in the v4 fold that needs flagging.

For #2, the frozen fields check is confirmed: module_id, role kind, concurrency, and control_ops are all locked at registration and rejected if modified, with the source code backing this up across forwarding, registry, and control modules.

For #3, the P2 semantics and honest threat model are documented—the ownership verification uses prefix-to-module-id mapping with spawn nonce validation, and v4 explicitly chose to document that same-user squatting remains in scope rather than implement per-connection tokens, which is an acceptable resolution.

For #4, the fed-state to CallError mapping table covers the pre-intent-crash scenario: if a fed module crashes before intent fsync, recovery finds no intent row and creates a not_sent tombstone, closing that hole. The finding also flagged misleading "reports accepted" language and the writer-accept boundary timing, which appears to be cut off mid-thought.

For #5, the effect_id incarnation epoch is defined as (origin_device_pubkey, incarnation_uuid, seq) with incarnation minted on db creation/loss/restore and seq high-water fencing—this is confirmed in the changelog.

For #6, retention is co-defined with outcome confirmation piggybacked on ack advancement and a bounded grace period; post-expiry re-arrivals get a typed ambiguity refusal. The finding asked for a concrete number, but v4 specifies only "bounded grace" without the actual value, which might be a partial resolution.

For #6a, the llm-runner appeal is dropped per the changelog, but I'm checking whether any stale claims about llm-runner's "proven" discipline remain in the document body or if that's just historical context in the v2→v3 changelog section.

For #7, harness registration is documented as a required provider-side coordination item with defined config posture and a verify-before-phase-2 gate, so that's closed.

For #8, the TOFU ceremony covers first contact gating, key rotation, and code binding both endpoints' static keys with residuals documented—all four fix-direction items are matched.

For #9, partition determinism is handled by the fed-module's keepalive reaper as the authoritative classifier, which closes loopback connections to deliver deterministic route-GOODBYEs while the module-direction GOODBYE remains best-effort by design.

For #10, the federation handshake exchanges raw capability documents and filters them down to the negotiated version before constructing the typed manifest, with unknown roles and fields dropped at the raw layer via closed enum deserialization.

For #11, topology is decided as one loopback connection per peer-module pair, confirmed by the register_module_connection logic that evicts prior connections on the same pair.

For #12, ClientHello device identity is a v2 transport addition for the federation/leaf path that's now required in phase 4+.

For #13, the Fork Cat contradiction about catalog-sync granularity is resolved—the mechanism is P1 `catalog.update` per peer-module connection, and the old coarse re-HELLO text has been removed.

But there's a critical new contradiction: §4.1 claims "removed tools get route-GOODBYE" which directly contradicts §2.6 and the changelog that state removed tools trigger a module-side typed error, never a GOODBYE. This is the exact defect #1 was about, and it means #1's fold is only partial—the normative fix in §2.6 is correct, but §4.1 still restates the architecturally impossible behavior, making this a load-bearing data-flow section that contradicts the core finding.

I need to verify the GOODBYE claims throughout the doc and check whether there are any remaining stale assertions about reports being accepted only after intent is durable.
Now let me verify a few potential stale/contradictory statements across the v4 doc — the GOODBYE claims, the "proven" appeal, and "accepted" language.

[assistant] 2026-07-05T10:35:01Z
I've spotted a clear contradiction in the documentation. Line 130 in section 4.1 states that removed tools receive a route-GOODBYE message, but this directly conflicts with the v4 core decision at line 85 that explicitly prohibits tool-granular GOODBYE messages. This stale statement wasn't updated during the v4 revision and contradicts multiple other sections including the changelog and decision log.

Let me verify the exact wording at line 130 to confirm this inconsistency.
I've confirmed a key contradiction. Let me read the exact text of 4.1 line 130 and check the 6.1 recovery reconciliation / retention details once more.

[assistant] 2026-07-05T10:35:07Z
I've found a clear contradiction across multiple sections of the documentation. Line 130 in §4.1 claims that removed tools receive a route-GOODBYE response, but this directly conflicts with what's stated in §2.6, the changelog, and the decision log—all of which specify that calls to removed tools should result in a module-side typed error instead, never a GOODBYE.

The issue is that §4.1 line 130 contains outdated v3 language that wasn't updated when §2.6 was revised. This means Finding #1 remains only partially resolved because the stale contradictory statement is still present in the document.

I'm now systematically checking the other findings. I notice line 23 in the v2→v3 changelog still references "borrowing llm-runner's proven intent-log discipline," but #6a requested dropping that "proven" appeal. The v4 changelog explicitly states this reputation appeal was dropped, and §6.1 doesn't rely on it anymore. Since line 23 is a historical changelog entry describing what v3 did, it's technically stale but exists in a historical context. This feels like a minor residual note rather than a true contradiction—#6a was low severity anyway, so I'd consider it closed.

Now I'm working through each finding systematically. For #1, the v4 fold correctly implements option (2) in §2.6 line 85, explaining that tool-granular GOODBYE is impossible because routes only bind module endpoints and channels, not tool identities. The code structure confirms this—RouteBinding carries module_id, endpoint, and channel with no tool identity. However, §4.1 line 130 still claims removed tools get route-GOODBYE, which directly contradicts the normative section. So the fold is only partial—the main section closes it correctly but the stale contradiction remains.

For #2, the concurrency and control_ops freeze is properly implemented. The v4 fold correctly states these are HELLO-time properties that get frozen, and the source confirms this: manifest_concurrency is read at register and stored on ModuleConnection, while control_ops flow through the registry and health prober. The design is consistent here. But there's a subtle issue: ProviderRole::ToolProvider has concurrency as a field inside the provides list, yet the frozen-field rule says concurrency can't change. So P1 updates claiming to only modify the provides list would actually need to exclude concurrency from that modification.

The validator handles this by comparing the new provides' concurrency against what's stored and rejecting if it differs—implementable but requires reaching into nested fields. The design explicitly names concurrency as frozen, so it's internally coherent; I'll mark this CLOSED with an implementation caveat. Control_ops is separate—it's derived from the HELLO's control_ops field, not part of the manifest provides list.

For #3, the prefix semantics are clear: reserved prefixes must end with `:` and match via `starts_with()`, exact-id reservations take precedence, and overlapping prefix owners are rejected at config load. Ownership is enforced by requiring the HELLO to present the owner module's current spawn nonce, not a nonce for the claimed id itself. The threat model acknowledges P2 isn't a same-user barrier—a same-user process on the same host can already read the connection.

key and owner's nonce, so P2 only protects against accidental collisions and lower-trust/different-user processes, which is documented honestly. The fed-module's per-peer-namespace creation under `fed:` is policy-driven, not P2-enforced. Looking at `reserved_hello_authorized` in supervise.rs, it's an exact module_id→nonce map that returns true on miss, and the v4 fold takes the documented-limitation path rather than adding SO_PEERCRED, explicitly stating the nonce-based check is defeatable by a same-user reader within the accepted trust floor. This is consistent. Now I need to verify whether the fold quietly weakens any locked decision—P2 was originally framed as protection against peer-namespace squatting.

Actually, v3 finding #3 explicitly demanded this honesty about P2 not being a same-user barrier, so the v4 clarification is the required fix, not a regression. That's resolved.

For the CallError taxonomy issue, the v4 fold maps fed-state to CallError outcomes in a table covering the pre-intent-crash hole: if the fed-module crashes before intent fsync, the origin sees OutcomeUnknown initially, but recovery finds no intent row and emits a durable not_sent tombstone, letting the caller re-invoke safely. The table also covers intent-durable and outcome-durable cases, with test vectors required to cover each row crash-cut style. This implements the fix direction directly through recovery reconciliation.

There's a transient window where the consumer sees OutcomeUnknown and won't auto-retry until fed-module restart and recovery, but that's documented. For a plain subc client without a tombstone-query API, the safe fallback is re-invocation: since no intent row exists, no effect_id was assigned, so re-invoking creates a fresh effect with no duplication risk. That closes it without requiring new client APIs.

For effect_id durability across restarts, the incarnation UUID is minted fresh whenever the origin's send-log database is created or restored, and stored in that database itself—so post-loss origins can never re-mint colliding IDs. The serving side maintains a per-(pubkey, incarnation) high-water mark and refuses any seq at-or-below it unless it's an exact dedup hit, which implements the fix. A fresh incarnation starts seq at 1 again but with a new incarnation_uuid, so the tuple never collides with old sequences.

For dedup-ledger retention, a row is kept until the origin confirms outcome-received via a piggybacked ack that advances a per-origin confirmed-watermark, plus a bounded grace period. After expiry, a re-arrival gets a typed ambiguity refusal rather than re-dispatch or a fabricated outcome.

The circularity is broken by tying retention to the origin's confirmed-watermark—a concrete signal—instead of an undefined "max re-send horizon." The grace-period number itself is still a tuning parameter to be pinned, but the actual defect (the circular definition) is closed.

For the llm-runner reputation appeal, it's dropped entirely. The mechanics rely on standard WAL discipline—fsync intent before the first network write and fsync outcome before replying—which stand on their own without needing to invoke runner reputation.

There's a stale reference in the v2→v3 changelog (line 23) that still mentions "borrowing llm-runner's proven intent-log discipline," but that's a historical note describing what v3 did, not a normative contradiction. The current spec (§6.1) has already dropped the reputation appeal, so I'm marking this closed with a minor housekeeping note about the residual language.

For #7, the federated harness story is being addressed: §5.4 introduces a first-class fed:<peer-fingerprint> harness class where providers validate against allowlists.  The config posture treats fed: binds the same as mcp:* unless the user's local config grants more access. The actual AFT verification is appropriately deferred to the phase-2 gate, which aligns with the v3 synthesis recommendations. This is closed as a documented coordination item.

For #8, the TOFU first-contact and rotation ceremony are both specified: first contact is gated until the out-of-band verification code is confirmed in the CK app on both ends, and key rotation must be signed by the old key or confirmed via an already-verified device—any other key change triggers first-contact handling.

For #9, the partition settlement mechanism makes the fed-module's keepalive reaper the authoritative classifier, and when it declares a peer partitioned, it closes that peer's loopback connections to deliver deterministic route-GOODBYEs to every consumer, while the subc-module GOODBYE remains best-effort and never relied upon for cleanup.

Cross-checking the source confirms that when the fed-module closes its connection to subc, the client-directed GOODBYE does escalate to close, so consumers get deterministic teardown. The shorthand "GOODBYE-on-partition" accurately describes the phase-3 deliverable since the consumer does receive a GOODBYE via connection close, even though the mechanism is connection-close-driven rather than module-direction GOODBYE-driven.

Now moving into #10 about the closed ProviderRole enum...

the federation handshake exchanges raw capability documents and performs version negotiation before the fed-module filters and translates down to the negotiated version, dropping unknown roles at that raw layer before constructing the typed manifest for subc-core. The manifest code confirms ProviderRole is a closed enum where unknown role tags fail serde decode, so the fold implements the fix exactly as needed.

For #11, the topology decision is that one loopback connection opens per exported remote module, each carrying exactly one HELLO, which aligns with subc-core's semantics where one registration per connection means multi-HELLO-per-connection was never viable.

#12 defers ClientHello device-identity as a phase-4+ transport addition, not a phase-0 blocker, which is appropriately acknowledged.

#13 confirms the Fork Cat contradiction is fixed — catalog-sync granularity is now resolved to per-(peer, module) connection updates with only the staleness-window number remaining open.

But v4 introduced a new contradiction: §4.1 line 130 says "removed tools get route-GOODBYE" while §2.6 line 85, the changelog, and the decision log all say there's never a tool-granular GOODBYE. This is stale v3 text that v4's fold of #1 should have caught and updated, making #1's resolution only partial. Line 130 is the only place this contradiction appears, so it's isolated to that one section.

On weakened decisions: P2's threat model isn't a weakening since it was never locked as a hard barrier—it's a required correction. P1's withdrawal of tool-granular GOODBYE isn't a regression either, since that promise was architecturally impossible anyway; the council endorsed the corrected option. The functional shift to module-side typed errors instead is an accepted residual, not a real capability loss.

For Phase 0, #1 is PARTIAL—the normative spec in §2.6 is correct and buildable, but §4.1:130 needs a one-line fix to remove the stale contradiction. An implementer building from §2.6 would get the right thing. #2 through #6 are all CLOSED and buildable.

The only blocker is that stale §4.1:130 line, which is a documentation defect in the federation-module narrative, not a subc-core P1/P2 spec defect. Since Phase 0 builds subc-core primitives and §2.6 is unambiguous, I'm leaning toward GO conditioned on fixing that one line—but the instruction's severity framing makes this a PARTIAL finding. I'm verifying the remaining edge cases—the keepalive and GOODBYE patterns in lines 253 and 268 align with the partition mechanism in §6.2, so those aren't contradictions. And the "in-flight routes to unchanged tools are undisturbed" clause in §4.1:130 is actually consistent with P1; only the stale "removed tools get route-GOODBYE" part needs correction. Since the registry currently has no update method and P1 will require a new one, that's expected scope, not a blocking finding. I have enough to finalize the verdict.
I have all the evidence I need. Both the v4 doc and the v3 synthesis are fully read, and I've verified every source-grounded finding against the actual subc-core/subc-protocol lines. One significant stale contradiction is confirmed.

[assistant] 2026-07-05T10:37:37Z
Now I'm working through my findings one by one. The first one concerns whether tool-granular GOODBYE is possible in v4, and according the spec at section 2.6, it's not — the routing system binds module endpoints to channels, and tool names only exist in opaque message bodies that the core never inspects. When a call reaches a removed tool, the module itself returns a typed error, which matches the current local behavior. The changelog confirms this, and I can see it reflected in the forwarding code.

But there's a problem: section 4.1 at line 130 still claims that removed tools get route-level GOODBYE, which directly contradicts what 2.6 says. That's a stale contradictory statement left in the document, so finding #1 is only partially resolved, not fully closed.

For finding #2, I'm checking the concurrency and control_ops inconsistency. The spec confirms these are HELLO-time properties that size the flow-control window and drive the health prober, and any payload attempting to change them gets rejected as a frozen field violation. I can verify this through the forwarding code where the default managed window is set.

Now for finding #3, I'm examining the P2 prefix semantics, connection ownership, and the nonce-bearer threat model. The spec lays out the syntax rules clearly—prefixes must end with a colon, exact IDs take precedence, overlapping owners are rejected at config load. The ownership model ties each prefix to a supervised module that must present the current spawn nonce on HELLO. The honest threat model acknowledges that P2 isn't a same-user barrier since a same-user process can read the connection key and environment nonce directly.

The v3 synthesis offered two paths forward: either implement SO_PEERCRED with per-connection tokens or explicitly document that P2 doesn't protect against same-user attacks. v4 chose documentation, which aligns with the synthesis's own acceptable resolution. There's one design detail to note—the spec says the OWNER module's current spawn nonce should be presented, not a nonce for the claimed ID, but the current reserved_hello_authorized logic keys on the claimed module_id instead. Since P2 is a new primitive being specified here, this is a design requirement that's now implementable.

For #4, I'm looking at the accepted-after-intent-durable versus the four-variant taxonomy and how it handles pre-intent crashes. The mapping shows that before intent fsync, the outcome is unknown at the time, but on restart recovery finds no intent row, proving the effect never left, so recovery emits a durable not_sent tombstone allowing free re-invocation. Once intent is durable, queries serve the ledger. Once outcome is durable, the state is terminal.

The core hole v3 identified: the writer accepts the body and crashes before fsyncing the intent, leaving the origin seeing OutcomeUnknown and unwilling to retry, even though nothing was recorded durably—a lost-but-unretryable situation. v4's fix through recovery reconciliation handles this: on restart, finding no intent row means the effect provably never left, so the fed-module can emit a not_sent tombstone or the caller can simply re-invoke since no effect_id was ever minted.

v4 chose the "at minimum" path from the v3 fix options: adding the explicit fed-state to CallError mapping table with test vectors mirroring real_daemon patterns. This resolves the concern, though there's a subtle gap—the caller sees OutcomeUnknown in both the "before intent fsync" and "intent durable, send unconfirmed" cases, so it can't blindly re-invoke without querying the reconciliation result to know which window it's in. The mechanism exists through the tombstone query, but the caller needs to consult that before deciding whether re-invocation is safe.

The core hole (lost-but-unretryable with no recovery path) is closed by the recovery reconciliation, tombstone query, and high-water fencing mechanism. I'm rating this CLOSED with a minor caveat about the imprecision in the "simply re-invokes" parenthetical—the actual mechanism is there, it's just not as straightforward as the wording suggests.

Now looking at finding #5: the effect_id structure uses a combination of origin device pubkey, incarnation UUID, and sequence number. The incarnation UUID is minted fresh whenever the origin's send-log database is created (on install, loss, or restore) and stored in that database, so a post-loss origin can never mint IDs that collide with pre-loss effects. The sequence is monotonic within an incarnation, and the serving side maintains a per-(pubkey, incarnation) high-water mark that refuses any sequence at or below it unless it's an exact dedup match—this prevents replay across restarts.

For #6, the dedup-ledger retention window is now co-defined: a row stays until the origin confirms outcome-received (via piggybacked ack advancing a per-origin confirmed-watermark) plus a bounded grace period. After expiry, a re-arrival gets a typed ambiguity refusal rather than re-dispatch or fabrication. The circularity is resolved by this co-definition, though v4 leaves the specific grace number unspecified—it's a tuning parameter like other open constants, so I'm marking this CLOSED with a minor note.

On #6a, the llm-runner "proven" claim is dropped entirely. The mechanics now stand on standard WAL discipline without needing that appeal.

But there's a stale reference: the v2→v3 changelog at line 23 still says "borrowing llm-runner's proven intent-log discipline." The active §6.1 text no longer invokes this (line 196 removed it, line 11 explicitly drops it), but the exact phrase "proven" that #6a flagged survives in the historical changelog section. Since it's a historical record describing what v3 did at the time, I'm leaning toward PARTIAL—the active text is clean but the flagged phrase lingers in the changelog. Low severity given it's explicitly historical, but I should flag it as PARTIAL to be rigorous.

For #7, the v4 text at line 180 directly addresses the provider-registration issue: it acknowledges the allowlist problem, defines a config posture (fed-class gets untrusted/project-tier treatment like mcp:*), and explicitly marks AFT verification as a required phase-2 gate. The language matches the v3 fix direction exactly. This is a SHOULD-FIX scheduled for phase 2, so it's addressed at the design level, but it's not actually resolved yet.

For #8, the TOFU mechanism is fully specified: first contact is gated until the out-of-band code is verified, rotation uses the old key's signature or an already-verified device, and the code itself derives from both endpoints' long-term static keys. All three sub-issues from v3 are covered and documented. CLOSED.

For #9, the partition settlement mechanism in v4 §6.2 establishes the fed-module's keepalive reaper as the authoritative classifier. When it declares a peer partitioned, it closes that peer's loopback connections, which triggers deterministic route-GOODBYEs to every consumer of that peer's tools (with OutcomeUnknown for in-flight requests). The subc module-direction GOODBYE itself is best-effort under partition conditions.

The forwarding code confirms that module-level GOODBYEs are best-effort and never close, but the connection-granular cleanup from connection-close does escalate to client-direction GOODBYEs that trigger close-on-delivery-failure. This matches the v3 fix direction exactly. The shorthand references in phase 3 and the decision log aren't contradictions—they're just condensed language for the underlying mechanism. CLOSED.

For #10, the closed ProviderRole enum can't skip unknown roles during serde decode, but v4 §6.5 handles this by having the federation handshake exchange raw capability documents in JSON, perform version and capability negotiation, and then filter/translate the raw document down to the negotiated version before constructing the typed local manifest for subc-core. Unknown roles, operations, and fields are dropped at that raw layer, so a newer peer can't break an older one.

For #11, the topology is now explicitly decided: one loopback connection per (peer, remote module) pair, each carrying exactly one HELLO. The register_module_connection function evicts any prior registration on the same connection_id, which means multi-HELLO-per-connection was never viable anyway. This matches subc-core's semantics and the v3 fix direction. CLOSED.

For #12, ClientHello needs to carry a device identity so per-device keying and revocation become implementable, though the spec notes this is still underspecified for phase 4 and beyond.

For #13, the old "coarse re-HELLO" language has been removed entirely and replaced with the correct mechanism: P1 catalog.update per (peer, module) connection. The changelog confirms this contradiction is fixed, and the historical reference to "coarse re-HELLO" only appears in the v2→v3 changelog section now.

But I'm finding a serious new contradiction: §4.1:130 states "removed tools get route-GOODBYE," which directly contradicts §2.6:85 and the changelog that say removed tools get a module-side typed error instead. This is the exact blocker from finding #1 resurfacing in a different section — a high-severity issue that makes #1 only partially resolved.

There's also a minor inconsistency around §6.1:196 dropping "proven" from the llm-runner appeal while the changelog still references it, but that's more of a residual historical note than a hard contradiction.

For weakened decisions, the P2 threat model explicitly stating it's "NOT a same-user barrier" isn't actually a regression — it's the sanctioned resolution path that was already agreed upon in v3. The "bounded grace" retention without a concrete number is a slight under-delivery on v3's request, but it's consistent with other open tuning parameters and not a true weakening of a locked decision.

Now looking at the phase-0 verdict: blockers #2 and #3 are closed, but #1 is partial. The §2.6 P1 spec itself is correct and implementable — a developer building from that section would create the right thing. However, §4.1:130 directly contradicts this by re-asserting the impossible "removed tools get route-GOODBYE" promise. The tension is that the P1 primitive code is correctly specified, but the documentation contains a contradictory statement about federation-module behavior that belongs in phase 1+.

The real question is whether this documentation defect blocks phase 0. Since the authoritative specs in §2.6 are correct and source-verified, and the §4.1 contradiction is just a stale line that needs fixing before phase 1, I'm leaning toward conditional GO — but I'm wrestling with whether a confirmation gate that surfaces a live contradiction re-asserting the original blocker should actually be a hard NO.

My judgment: the primitive design is sound, the contradiction is cosmetic but must be fixed, and blocking all of phase 0 on a one-clause doc correction would be disproportionate. So I'll issue GO for building the P1 and P2 specs, with the §4.1 fix as a mandatory precondition that gets flagged prominently.

Let me verify the key line citations to anchor my verdict. Verified the final citation for register_with_control_ops duplication in registry.rs, and all sources check out.
All evidence gathered and cross-checked against source. I verified every source-grounded finding against the actual subc-core/subc-protocol lines cited. Below is the per-finding confirmation.

<COUNCIL_MEMBER_RESPONSE>
# subc Federation Design v4 — CONFIRMATION RE-GATE

Scope: confirm each of the 13 v3 findings is actually closed by v4's fold; verify source-grounded folds against real code (which I read: registry.rs, forwarding.rs, control.rs, supervise.rs, subc-protocol/manifest.rs). Locked skeleton not re-litigated.

## Per-finding verdicts

**#1: PARTIAL** — 2.6:85 ("No tool-granular GOODBYE… a call to a since-removed tool therefore reaches the module and gets a module-side typed error… P1 promises catalog VISIBILITY freshness, not route teardown") + changelog:9 correctly retract the impossible promise, and this matches source: `RouteBinding`/`ModuleRouteKey` carry only `module_id` + `channel`, no tool identity (forwarding.rs:43-60, confirmed). **BUT 4.1:130 still reads "removed tools get route-GOODBYE"** — the exact impossible promise finding #1 killed, re-asserted verbatim in the data-flow narrative. The authoritative spec closes it; a stale contradictory sentence survives → PARTIAL, not CLOSED. (See NEW-CONTRADICTIONS.)

**#2: CLOSED** — 2.6:86 freezes `module_id, role kind, concurrency, control_ops` as HELLO-time and rejects any P1 payload changing them (`catalog_update_frozen_field`). Source-verified as load-bearing: concurrency sizes the credit window (forwarding.rs:19/22 `DEFAULT_MODULE_MANAGED_WINDOW=32` vs `STATELESS_PARALLEL_WINDOW=1024`), read via `manifest_concurrency` at register (control.rs:619) and stored on `ModuleConnection` (forwarding.rs:304); `control_ops` via `effective_module_control_ops` (control.rs:584), stored registry.rs:83, read by the health prober (control.rs:1260). Freeze-and-reject is exactly the converged fix; no contradicting statement elsewhere.

**#3: CLOSED** — 2.6:89-91: prefix MUST end with `:`, `id.starts_with(prefix)` (so `fed:`≠`fedx:tool`), exact-id precedence, overlap rejected at config load, boundary matrix (89); prefix→owner supervised module_id verified against the OWNER's current spawn nonce (90); honest threat statement "P2 is NOT a same-user barrier" (91). Source-verified: `reserved_hello_authorized` is an exact-id lookup returning `true` on miss (supervise.rs:389-390 `None => true`), and the nonce ships via `SUBC_LAUNCH_NONCE_ENV` (supervise.rs:2033) — same-user readable. v4 takes the synthesis's explicitly-sanctioned "document P2 is not a same-user barrier" branch, so honest, not weakened.

**#4: CLOSED (minor caveat)** — 6.1:200-206 fed-state→CallError table + recovery reconciliation closes the pre-intent-crash hole: "before intent fsync → recovery finds NO intent row → effect provably never left → durable `not_sent` tombstone" (203); "intent durable, send unconfirmed → queries SERVING ledger" (204); test vectors required per row, crash-cut style (206). This is the mechanism (not just prose) the synthesis asked for. Caveat: the parenthetical "the caller simply re-invokes" (203) slightly overstates — the consumer sees `OutcomeUnknown` in both the before-intent and intent-durable windows and must consult the tombstone/reconciliation to distinguish them; the query mechanism is specified, so the hole is closed, but "simply re-invokes" is imprecise.

**#5: CLOSED** — 6.1:197 `effect_id = (origin_device_pubkey, incarnation_uuid, seq)`, incarnation minted on db create/loss/restore and stored IN that db; serving side keeps per-(pubkey,incarnation) high-water mark and refuses seq regression with a typed fence (never replay). Directly implements the converged fix (incarnation epoch + serving-side fence). New mechanism, no in-repo source to contradict.

**#6: CLOSED (grace number still open)** — 6.1:199 co-defines retention to the origin's confirmed-watermark (piggybacked ack) + bounded grace; post-expiry = typed `effect_outcome_expired` refusal, never re-dispatch; residual documented. Circularity resolved. Caveat: synthesis asked for "a concrete number"; v4 says "bounded grace" without one — a tuning param left open like the Fork Cat staleness number, acceptable but note it.

**#6a: PARTIAL** — 6.1:196 + changelog:11 drop the appeal ("standard WAL discipline… stand on their own"). **BUT line 23 (v2→v3 changelog) still reads "borrowing llm-runner's proven intent-log discipline"** — the exact "proven"/external appeal #6a wanted removed survives in a historical section. Low severity, but the flagged phrase persists → PARTIAL.

**#7: CLOSED (deferred as designed)** — 5.4:180 first-class `fed:<peer-fingerprint>` harness class; acknowledges AFT allowlist `{opencode,pi,runner,mcp:*}` would reject/unscope `fed:*`; defines config posture (fed-class = untrusted/project-tier like `mcp:*`); "Verify against real AFT before phase 2." Matches the converged fix and its own phase-2 gate; AFT is external so residual-unverified is expected and disclosed.

**#8: CLOSED** — 5.3:170-174: first contact non-routable until OOB code compared (172); rotation must be old-key-signed tombstone chain or verified-device/manual re-pair, else presents as first contact (173); code binds BOTH endpoints' long-term device static keys, never session/ephemeral (172); residual documented (174). All four converged sub-fixes present.

**#9: CLOSED** — 6.2:213 makes the fed-module keepalive reaper the AUTHORITATIVE classifier that closes the per-peer loopback connection (→ connection-granular cleanup → deterministic client-direction GOODBYEs), and demotes module-direction GOODBYE to best-effort. Source-verified: `GoodbyeTargetKind::Module` = "best-effort DROP, never close" (forwarding.rs:68-93), while client-target GOODBYE escalates to close (`close_on_delivery_failure`, forwarding.rs:112-113). Mechanism is sound. (Shorthand "GOODBYE-on-partition" at :253/:268 is a label, not a contradiction.)

**#10: CLOSED** — 6.5:223 exchanges raw capability docs and filters/translates to the negotiated version BEFORE typed decode/P1. Source-verified: `ProviderRole` is a closed `#[serde(tag="role")]` enum, "unknown role tags fail serde decode" (manifest.rs:36-39). Raw-layer filtering is the correct and only viable fix.

**#11: CLOSED** — 2.5:79 decides "one loopback connection per (peer, remote module)… exactly one HELLO," and phase-1:251 mandates the N-connections-one-process confirming test. Source-verified: `register_module_connection` evicts the prior endpoint for the same connection_id (forwarding.rs:280-282), so multi-HELLO-per-connection was never viable — v4's topology matches the eviction semantics.

**#12: CLOSED (deferred)** — 6.4:220 + changelog:17 confirm ClientHello device-identity is a v2 transport addition, phase-4+. The field-vs-message residue is unresolved but the finding is a NOTE explicitly not gating phase-0; deferral is appropriate.

**#13: CLOSED** —  Fork Cat:243 now reads "RESOLVED mechanism: P1 catalog.update per (peer, module) connection (2.6). Open: only the acceptable staleness-window number." The old "coarse re-HELLO" text is gone; grep confirms "coarse re-HELLO" survives only at line 21 (v2→v3 changelog) where it is correctly stated as withdrawn/not-viable.

## NEW-CONTRADICTIONS (b-class)

1. **[HIGH] 4.1:130 vs 2.6:85 + changelog:9 — GOODBYE for removed tools.** 4.1:130: "removed tools get route-GOODBYE." 2.6:85: "No tool-granular GOODBYE… a call to a since-removed tool… gets a module-side typed error." changelog:9: "never a GOODBYE." This is the flagship #1 blocker re-surfacing verbatim in the data-flow section. The authoritative primitive spec (2.6) is correct and source-consistent, but 4.1:130 would mislead a phase-1 implementer into re-attempting the architecturally-impossible tool-granular GOODBYE. **Must-fix: delete/replace the "removed tools get route-GOODBYE" clause at 4.1:130.**

2. **[LOW] line 23 vs changelog:11 / 6.1:196 — "proven" llm-runner appeal.** changelog:11 and 6.1:196 drop the external "proven" appeal; line 23 (historical changelog) still says "borrowing llm-runner's proven intent-log discipline." Residual, historical-section only; harmless but the exact flagged phrase persists.

## WEAKENED-DECISIONS (c-class)

None material. Two items examined and cleared:
- **P2 "not a same-user barrier" (2.6:91)** is not a quiet regression — the v3 synthesis (#3 fix direction) explicitly sanctioned documenting P2 as a different-user/accidental-collision barrier under the accepted same-host floor. This is honest scoping, not a weakening of the locked exact-id-reservation guarantee.
- **P1 "provides-list-only" (2.6:84-86)** narrows 2.6's "non-disruptive catalog update" but is the sanctioned resolution of #1/#2, not a regression of a locked decision.
- Minor: 6.1:199 "bounded grace" and Fork Cat staleness number remain un-numbered — open tuning params, consistent with the doc's stated open items, not weakened decisions.

## PHASE-0 VERDICT: GO (conditional on one trivial doc fix)

Justification: Phase 0 builds the P1 `catalog.update` and P2 prefix-reservation primitives, whose authoritative specs live in 2.6. Both are correct, implementable, and source-verified: #2 (frozen fields — concurrency/control_ops confirmed load-bearing in forwarding.rs/control.rs/registry.rs) and #3 (delimiter semantics + owner-module nonce mapping + honest same-user threat model — confirmed against `reserved_hello_authorized`'s true-on-miss exact lookup and the env-borne launch nonce) are fully CLOSED and would not bake incorrect registry/forwarding invariants. #1's actual primitive design (provides-list-only, no tool-granular GOODBYE, module-side typed error) is also CLOSED in 2.6 and consistent with the tool-less `RouteBinding`. The one blocker to full confidence is **not a design flaw in the artifact phase 0 builds** but a stale contradictory sentence at 4.1:130 that re-asserts the impossible tool-granular GOODBYE promise — a one-clause documentation defect in a phase-1 narrative, not in the 2.6 primitive spec. I am NOT softening this: 4.1:130 must be corrected (and ideally line 23's "proven" residue scrubbed) because leaving the flagship blocker's exact wording live in the doc risks a phase-1 implementer reviving it. GO for writing P1+P2 code now, hard-conditioned on fixing 4.1:130 so the doc no longer contradicts its own locked P1 semantics.
</COUNCIL_MEMBER_RESPONSE>

