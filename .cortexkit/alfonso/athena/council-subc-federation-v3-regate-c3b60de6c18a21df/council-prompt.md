
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

You are an adversarial reviewer performing a v3 RE-GATE of the subc federation design. Your job is to VERIFY that v3's deltas actually close the prior v2 re-gate findings, and to HUNT NEW gaps introduced by the deltas themselves. Be skeptical, precise, and source-grounded. Do NOT rubber-stamp.

## READ FIRST (in full)
The design doc: `docs/subc-federation-design.md` (v3). Read the whole file. Pay special attention to §2.6 (two subc-core primitives P1/P2), §5.3 (TOFU), §5.4 (identity split), §6.1 (at-most-once mechanics), §6.2 (partition/liveness).

## SOURCE GROUNDING (already verified by the orchestrator — you may re-open any file to confirm; cite file:line where you rely on source)
Key subc-core facts relevant to the primitives:

1. **Concurrency / flow-control coupling (critical for P1).**
   - `Registry::register_with_control_ops` (crates/subc-core/src/registry.rs:65-89): rejects duplicate `module_id` (line 74-75); stores `control_ops` taken from the HELLO; bumps `generation`.
   - control.rs:584 — `control_ops` are computed from the HELLO's advertised ops (`effective_module_control_ops(hello.control_ops)`), i.e. control_ops are a HELLO-time property.
   - control.rs:619-625 — at registration, `concurrency = manifest_concurrency(&registration.manifest)` is read from the manifest and passed into `forwarding.register_module_connection(...)`.
   - forwarding.rs:271-308 — `register_module_connection` takes `concurrency: Concurrency`, and (line 280-282) EVICTS any prior endpoint for that connection_id, then (line 284) bumps `next_generation`, creating a NEW `ModuleEndpointId { connection_id, generation }`. All existing route bindings key on the OLD `ModuleEndpointId` (forwarding.rs:41-46, 55). So re-registering a connection orphans its existing route bindings.
   - forwarding.rs:18-22 — the per-channel request-credit WINDOW is derived from concurrency: `DEFAULT_MODULE_MANAGED_WINDOW = 32` (ModuleManaged), `STATELESS_PARALLEL_WINDOW = 1024` (StatelessParallel). So concurrency in the manifest DIRECTLY sets the flow-control window. control.rs:1852-1862 `manifest_concurrency` — only `ToolProvider` carries explicit concurrency; other roles fall back to ModuleManaged (32).
   - IMPLICATION TO SCRUTINIZE: P1 `catalog.update` proposes to REPLACE a module's manifest IN PLACE without re-registering the connection. But concurrency AND control_ops are captured at register time from the manifest/HELLO. Does P1 change them? If the replacement manifest changes concurrency, does the live flow-control window change, and is that safe on in-flight routes with outstanding credits? If it does NOT change them, is the catalog now inconsistent with the flow window? Can catalog.update change advertised control_ops (which the health prober reads)? Is there any atomic transition that guarantees a route is NEVER bound to a tool the registry no longer knows?

2. **Reserved-nonce squatting protection (critical for P2).**
   - supervise.rs:344-395 — `SupervisorHandle` holds `reserved_nonces: HashMap<String,String>` (EXACT module_id → nonce) and `spawn_nonces`. `reserved_hello_authorized(module_id, presented)` (line 384-395): if there is NO reserved_nonces entry for the EXACT module_id, returns `true` (unconditionally authorized). Only an exact-id match triggers nonce checking.
   - IMPLICATION: P2 wants to extend this from exact ids to PREFIXES (e.g. reserve `fed:`). The gate today is a HashMap exact lookup returning `true` on miss. Prefix matching is a real semantics change. SCRUTINIZE: collision semantics between an exact reserved id and a reserved prefix (does `fed:` prefix-reserve also cover an exact id `fed`? what about `fedx:`?); what stops the fed-module ITSELF from registering `fed:<peerA>:tool` when policy meant a different module owns peerA; how is "connection owned by that module's attested process" KEYED, given the fed-module opens N per-peer loopback connections all from ONE attested process (one launch nonce) — is ownership keyed by nonce, by pid, by connection? Can a co-resident local key-holder open a connection and present the fed-module's launch nonce (is the nonce a shared secret visible to other local processes)?

3. **At-most-once client taxonomy (critical for §6.1).**
   - crates/subc-client-rs/src/consumer.rs:581-593 — `CallError` variants are EXACTLY: `NotSent` (request not accepted by writer path / route.open failed before data send), `OutcomeUnknown` (accepted by writer path but no terminal response observed), `Module(ErrorBody)` (handler error frame), `SubscriptionBackpressure`. Real-daemon tests (crates/subc-client-rs/tests/real_daemon.rs:294-303, 421-422, 524-525, 801) assert: accepted mid-call → OutcomeUnknown; OutcomeUnknown is NEVER auto-retried; bounded target absence → NotSent; route-gone after accept → OutcomeUnknown.
   - IMPLICATION: §6.1 says the fed-module reports `accepted` to the local consumer only after `intent` is durable, and an ambiguous WAN outcome surfaces as `OutcomeUnknown`. SCRUTINIZE: is "accepted-after-intent-durable" compatible with the existing NotSent/OutcomeUnknown taxonomy at the ORIGIN consumer (which is a plain subc client that only knows these 4 variants)? Once the fed-module has reported `accepted`/written the body, the origin client can ONLY see OutcomeUnknown on ambiguity — is that the intended and safe classification, and does anything the origin consumer does with OutcomeUnknown (never retry) compose correctly with the fed-module's own dedup/re-send? Also: llm-runner (the cited intent-log precedent) is NOT in this repo — treat that precedent as unverified/external.

4. Client taxonomy has no separate "durably-recorded-remote-intent" state; the fed-module must MAP its richer internal state machine onto these 4 variants for the origin consumer.

## PRIORITY FOCUS AREAS (address each explicitly)
1. **P1 catalog.update**: safe interaction with in-flight routes; the flow-control window when the replacement changes concurrency; route bindings to REMOVED tools; the catalog generation/staleness model; the health prober's advertised control_ops (can catalog.update change control_ops?). Is the daemon-side state transition atomic enough to NEVER leave a route bound to a tool the registry no longer knows?
2. **P2 prefix reservation**: collision semantics between exact reserved ids and prefixes; what stops the fed-module registering an id under a DIFFERENT module's reserved prefix; interaction with the connection-ownership check across the fed-module's N per-peer connections (all owned by one attested process — how is ownership keyed? is the nonce a shared secret?).
3. **§6.1 mechanics**: is the serving-side dedup ledger retention window soundly definable? What happens when the ledger row is evicted but the origin legitimately re-sends? Cross-restart durability of the monotonic seq in effect_id (a reset seq after origin db loss would collide with prior effect_ids)? Is reporting accepted-after-intent-durable compatible with the existing NotSent/OutcomeUnknown taxonomy at the ORIGIN consumer?
4. **Identity split §5.4**: is the profile-authored local BindIdentity sufficient for AFT-class providers that validate harness against an allowlist? (A `fed:<peer>` harness marker would be rejected by AFT today if AFT allowlists harness values — does the design need a harness-registration story? Check whether AFT/providers validate harness/BindIdentity against an allowlist.)
5. **TOFU §5.3**: first-contact window (TOFU trusts the first key blindly — what if the cloud is malicious at FIRST contact, before any pin exists?); key rotation legitimate-vs-attack disambiguation (how does a user tell a legit key rotation from an attack-substitution? both present as "changed key"); what the verification code binds EXACTLY (does it bind the session, the directory entry, both endpoints' long-term keys?).

Also sanity-check ANYTHING else that strikes you as unsound (e.g. §6.2 partition classification, §6.4 ClientHello device-identity transport addition, §6.5 cross-version negotiation, the one-connection-per-peer multi-registration claim vs forwarding.rs eviction behavior, relay DoS).

## OUTPUT REQUIREMENTS
- For EACH finding: give it a short title, a VERDICT of **BLOCKER** / **SHOULD-FIX** / **NOTE**, the evidence (cite file:line where you rely on source), WHY it matters, and a CONCRETE resolution proposal.
- Explicitly state, per v2-re-gate delta (P1, P2, §6.1, §5.4, §5.3), whether the v3 delta ACTUALLY CLOSES the prior finding or leaves residue.
- Separate "the delta closes the old finding" from "the delta introduces a NEW gap."
- END with an overall verdict: **GO** / **GO-WITH-CHANGES** / **NO-GO** for proceeding to phase 0 (building the two subc-core primitives P1 and P2), with a one-paragraph justification. If GO-WITH-CHANGES, list the must-fix-before-phase-0 items vs the can-fix-during items.
- Be concrete. Prefer "this specific line/mechanism is unsound because X; fix by Y" over generalities. Distinguish genuine BLOCKERs from nitpicks — calibrate severity honestly.