
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

ADVERSARIAL RE-GATE — subc-core dispatch redesign v2

You are one of several independent expert reviewers performing a concurrency-critical adversarial re-gate. Repo: subconscious at ~/Work/Projects/CortexKit/subconscious. Design under review: docs/subc-dispatch-redesign-v2.md (committed 72891b31). Shipped daemon source: crates/subc-core/ at master. This is v2 of a dispatch redesign; v1 (docs/subc-dispatch-redesign.md) received a UNANIMOUS 8/8 NO-GO with 10 blockers B1-B10. v2 claims to close all of them.

THE V1 ARCHIVE (READ IT FIRST for exact blocker statements + source citations):
.cortexkit/alfonso/athena/council-subc-dispatch-redesign-e26112ccb9170497/synthesis.md
The B1-B10 blockers map to findings #1-#10 in that synthesis (in order). Each v2 section tags which blocker(s) it claims to resolve.

YOUR TWO JOBS:
(A) VERIFY each v1 blocker B1-B10 is ACTUALLY closed by the v2 mechanism — not merely claimed. Read the v2 mechanism, then read the shipped source it cites, and confirm the mechanism genuinely closes the defect. A plausible-sounding paragraph is not a closure; trace the concurrency.
(B) HUNT for NEW defects the v2 mechanisms introduce.

GROUND TRUTH (already verified at source — you may re-verify):
- forwarding.rs:1692-1700 ChannelFlow::acquire does permit.forget() then in_flight.fetch_add — credit is NOT RAII-held by the task.
- forwarding.rs:1702-1731 release() has a CAS in_flight!=0 guard that ignores over-release (best-effort, not a security boundary per its own comment).
- router.rs:465-496 shipped handle_bound: acquire().await → mutate header → module_sink.send().await → release() on send error.
- router.rs:281-309 module->client terminal path: try_send to client, then flow.release() if terminal.

V2 KEY MECHANISMS TO SCRUTINIZE (map to doc sections):
1. §1 RouteDispatcher: route-local Mutex<RouteInbox> (VecDeque<corr> + HashMap<corr,Slot> + admission enum + outstanding count) as SINGLE serialization point; every op holds the lock O(1) and NEVER across an await; drain task's blocking awaits (flow.acquire, module_sink.send) happen OUTSIDE the lock. VERIFY: is every claimed-O(1)-under-lock op actually O(1) and await-free? Can the read loop and drain task deadlock/livelock on this lock? Is Notify usage correct (lost-wakeup between pop-returns-None and wait)?
2. §2 Per-corr state machine Queued→Claimed→Delivered with CANCEL decided under the same lock. VERIFY B2 limbo is TRULY gone: enumerate CANCEL arriving in EACH state incl. exactly at the Claimed/acquire boundary and the Delivered/send boundary; can a terminal still double-fire (synthetic cancelled + module terminal) or vanish (zero terminal)? The rollback path (Claimed{cancelled:true} after acquire → release + synthesize cancelled): does it race the module→client terminal path?
3. §2 outstanding/Delivered recorded UNDER LOCK BEFORE module_sink.send (B5). VERIFY a fast module terminal cannot arrive before corr is Delivered: trace exact happens-before. The frame isn't sent until after the lock releases with state=Delivered — but is the terminal-release path's slots.remove correctly ordered against a Delivered set before send?
4. §4 Credit RAII (AcquiredCredit guard) + drain error arms: ChannelFlowClosed disambiguation via per-route teardown:TeardownKind field. VERIFY the teardown field is set/read under the right lock ordering and the None→backend_error arm actually preserves the shipped test blocked_flow_control_acquire_wakes_when_module_tears_down (forwarding.rs ~3811). Can the teardown kind be stale/racing when the drain reads it?
5. §3 R11 exactly-once release: terminal path does slots.remove under lock, releases only if was-Delivered. VERIFY against shipped release() which ALREADY has a CAS in_flight!=0 guard (forwarding.rs:1702-1731) — does v2's gate + existing CAS double-guard CLEANLY or CONFLICT? Is the concurrent-duplicate case actually closed, or does the double-guard mask/hide a real leak?
6. §5 Teardown 3-phase Open/Closing/Closed: admission=Closing before snapshot removal; cancel_token select; bounded-join then abort. VERIFY: the try_push-after-flush hole (B9) — is admission=Closing genuinely happens-before any stale-snapshot reader's push? Can a drain task blocked on module_sink.send with a FrameSink clone still hang connection close (server.rs:241-267)? Is the JoinHandle ownership (teardown path, not binding) actually acyclic?
7. §7 Snapshot publish-under-lock + closed:AtomicBool recheck (B10): VERIFY the closed flag set-under-write-lock-before-publish + reader recheck restores today's unknown_channel observable (not backend_error) on stale-Bound. Is there any ordering where closed=false is read but the route is already released?
8. §6 Client-side-only control offload (B7): module connections NOT split, bind-ack stays inline. VERIFY the shipped bind barrier test accepted_route_publishes_route_open_before_immediate_reverse_request (router.rs:1078-1102) stays green AND that client-side route.open FIFO doesn't itself reorder anything observable.
9. §8 merge-0 SDK prerequisite (B1): the retryable data-plane class. VERIFY the classification claim against SDK source (client.ts classifyFailure ~781-792, consumer.rs ~570-579) — is a distinct retryable class actually needed and does the proposed in-place-retry (not reconnect) fit the existing managed-call retry loop? NOTE: broca/aft/alfonso-core are NOT in this checkout — their contract impact is UNVERIFIABLE; flag this.
10. §9/§11 The 3-merge rollout ordering + invariant deltas I3/I4/I7 — are they now HONEST against source, and is merge-1 truly standalone-landable under the publish-under-lock constraint?

ALSO CHECK §12 Q1'-Q5' open-question leans for wrong leans. AND HUNT ANYTHING NEW:
- lock-ordering inversions between the RouteInbox mutex and the global forwarding write lock (specify the hierarchy; is it respected everywhere?);
- the AcquiredCredit RAII guard interacting with the outstanding count → double-counting or double-release;
- Notify lost-wakeups (the classic pop-returns-None → miss notify_one → sleep-forever gap);
- the corr-uniqueness assumption (Q4') as a credit-leak vector if not enforced at enqueue.

DELIVERABLE (your COUNCIL_MEMBER_RESPONSE):
- For EACH v1 blocker B1-B10: state CLOSED / NOT-CLOSED / NEWLY-BROKEN, with a one-to-three sentence justification and source citation (file:line). "CLOSED" requires you traced the mechanism, not just read the claim.
- A section of NEW defects introduced by v2 mechanisms, each with severity (BLOCKER/MAJOR/MINOR), a concrete interleaving or source contradiction, and file:line citations.
- Rulings on Q1'-Q5' leans (RIGHT / WRONG / RIGHT-BUT-UNSAFE).
- Your bottom-line verdict: GO / GO-WITH-CHANGES (enumerate each change, specific + file:line-cited) / NO-GO (enumerate blockers).

Be adversarial and precise. Cite shipped source for every claim. A single un-refuted concurrency or contract defect is a NO-GO. Do not flatten disagreement with the v1 findings — if v2 genuinely closed a blocker, say CLOSED and prove it; if it only papers over one, say NOT-CLOSED and show the hole.